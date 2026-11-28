"""Authenticate a CI cache before restoring APT lists or archives."""

import datetime
import email.utils
import hashlib
import pathlib
import re
import shutil
import subprocess
import sys
import tempfile


COMPRESSION = (".lz4", ".gz", ".xz", ".bz2", ".zst", ".lzma")
SHA256 = re.compile(rb"[a-fA-F0-9]{64}\Z")
SIGNED_MESSAGE = b"-----BEGIN PGP " + b"SIGNED MESSAGE-----\n"
APT_HELPER = shutil.which("apt-helper") or "/usr/lib/apt/apt-helper"
UBUNTU_KEYRING = "/usr/share/keyrings/ubuntu-archive-keyring.gpg"


def paragraphs(stream):
    fields = {}
    field = None
    for line in stream:
        line = line.rstrip(b"\r\n")
        if not line:
            if fields:
                yield fields
            fields = {}
            field = None
        elif line.startswith((b" ", b"\t")) and field is not None:
            fields[field] += b"\n" + line
        elif b":" in line:
            field, value = line.split(b":", 1)
            field = field.lower()
            fields[field] = value.strip()
    if fields:
        yield fields


def apt_targets():
    # Query only local source configuration, never the untrusted lists.
    with tempfile.TemporaryDirectory() as empty:
        output = subprocess.check_output([
            "apt-get", "indextargets", "--no-release-info",
            "-o", f"Dir::State::lists={empty}",
            "-o", "Dir::Cache::pkgcache=", "-o", "Dir::Cache::srcpkgcache=",
        ])
    targets = {}
    releases = {}
    for stanza in paragraphs(output.splitlines()):
        name = pathlib.Path(stanza[b"filename"].decode()).name
        metakey = stanza[b"metakey"].decode()
        suffix = metakey.replace("/", "_")
        if not name.endswith(suffix):
            continue
        release = name.removesuffix(suffix) + "InRelease"
        target = (release, metakey)
        if targets.setdefault(name, target) != target:
            raise ValueError(f"ambiguous source for {name}")
        releases.setdefault(release, set()).add((stanza.get(b"release", b""),
                                                stanza[b"sourcesentry"].decode()))
    return targets, releases


def source_keyrings(entry):
    filename, _, number = entry.rpartition(":")
    source = pathlib.Path(filename)
    contents = source.read_bytes().splitlines()
    if source.suffix == ".sources":
        stanzas = list(paragraphs(line for line in contents if not line.lstrip().startswith(b"#")))
        signed_by = stanzas[int(number) - 1].get(b"signed-by", b"")
    else:
        options = re.match(rb"\s*deb(?:-src)?\s+\[([^]]*)\]", contents[int(number) - 1])
        signed_by = next((option.partition(b"=")[2] for option in options[1].split()
                          if option.lower().startswith(b"signed-by=")), b"") if options else b""
    if signed_by:
        rings = signed_by.replace(b",", b" ").decode().split()
        if not all(pathlib.Path(ring).is_absolute() and ring.endswith(".gpg") for ring in rings):
            raise ValueError(f"unsupported Signed-By in {entry}")
        return tuple(sorted(rings))
    trusted = pathlib.Path("/etc/apt/trusted.gpg")
    rings = sorted(trusted.with_suffix(".gpg.d").glob("*.gpg"))
    if trusted.is_file():
        rings.append(trusted)
    if not rings:
        raise ValueError(f"no trusted keyrings for {entry}")
    return tuple(map(str, rings))


def release_hashes(path, sources):
    suites = {suite for suite, _ in sources}
    rings = {source_keyrings(entry) for _, entry in sources}
    if len(suites) != 1 or len(rings) != 1:
        raise ValueError(f"ambiguous source for {path.name}")
    with path.open("rb") as stream:
        if stream.readline(len(SIGNED_MESSAGE) + 1) != SIGNED_MESSAGE:
            raise ValueError(f"not a clear-signed Release: {path.name}")
    command = ["gpgv", "--output", "-"]
    keyrings = rings.pop()
    for ring in keyrings:
        command.extend(["--keyring", ring])
    verified = subprocess.run([*command, "--", path], stdout=subprocess.PIPE, check=False)
    if verified.returncode != 0:
        raise ValueError(f"bad signature: {path.name}")
    fields, = paragraphs(verified.stdout.splitlines())
    suite = suites.pop()
    names = {fields.get(b"suite")}
    if UBUNTU_KEYRING not in keyrings:
        names.add(fields.get(b"codename"))
    if suite and suite not in names:
        raise ValueError(f"wrong suite: {path.name}")
    if expires := fields.get(b"valid-until"):
        expiry = email.utils.parsedate_to_datetime(expires.decode())
        if expiry.tzinfo is None or expiry <= datetime.datetime.now(datetime.timezone.utc):
            raise ValueError(f"expired Release: {path.name}")
    hashes = {}
    for line in fields[b"sha256"].splitlines():
        if not line:
            continue
        digest, size, name = line.split()
        if not SHA256.fullmatch(digest) or not size.isdigit():
            raise ValueError(f"invalid SHA256 entry: {path.name}")
        entry = (int(size), digest.lower())
        if hashes.setdefault(name.decode(), entry) != entry:
            raise ValueError(f"ambiguous SHA256 entry: {path.name}")
    return hashes


def sha256_file(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest().encode()


def read_index(path, metakey, extension, signed_hashes):
    # APT can recompress an index locally. In that case only the decoded
    # bytes have a hash in InRelease.
    if extension and (entry := signed_hashes.get(metakey + extension)):
        size, digest = entry
        if path.stat().st_size != size or sha256_file(path) != digest:
            raise ValueError(f"index SHA256 mismatch: {path.name}")
        return None
    size, digest = signed_hashes[metakey]
    if extension:
        with subprocess.Popen([APT_HELPER, "cat-file", path], stdout=subprocess.PIPE) as process:
            content = process.stdout.read(size + 1)
            if len(content) > size:
                process.kill()
            if process.wait() != 0:
                raise ValueError(f"cannot decode index: {path.name}")
    else:
        if path.stat().st_size != size:
            raise ValueError(f"wrong index size: {path.name}")
        content = path.read_bytes()
    if len(content) != size or hashlib.sha256(content).hexdigest().encode() != digest:
        raise ValueError(f"index SHA256 mismatch: {path.name}")
    return content


def archive_digests(content, wanted, digests):
    # Scan only stanzas for package names in the cache. The largest Ubuntu
    # index has many more packages than a CI job installs.
    for match in wanted.finditer(content):
        end = content.find(b"\n\n", match.end())
        fields = next(paragraphs(content[match.start():None if end == -1 else end].splitlines()))
        package, version, arch, digest = (fields.get(field, b"") for field in
                                          (b"package", b"version", b"architecture", b"sha256"))
        if version and arch and SHA256.fullmatch(digest):
            name = b"_".join((package, version.replace(b":", b"%3a"), arch)) + b".deb"
            digests.setdefault(name, set()).add(digest.lower())


def verify_lists(files, archives):
    targets, releases = apt_targets()
    signed = {name: release_hashes(path, releases[name]) for name, path in files.items()
              if name.endswith("_InRelease")}
    packages = sorted({path.name.split("_", 1)[0].encode() for path in archives})
    wanted = (re.compile(rb"(?m)^Package: (?:" + b"|".join(map(re.escape, packages)) + rb")$")
              if packages else None)
    digests = {}
    for name, path in files.items():
        if name in signed:
            continue
        extension = next((ext for ext in COMPRESSION if name.endswith(ext)), "")
        release, metakey = targets[name.removesuffix(extension)]
        content = read_index(path, metakey, extension, signed[release])
        if wanted and (metakey == "Packages" or metakey.endswith("/Packages")):
            if content is None:
                # The publisher's compressed file matched its signed hash.
                content = subprocess.check_output([APT_HELPER, "cat-file", path])
            archive_digests(content, wanted, digests)
    return digests


def verify_archive(path, digests):
    expected = digests.get(path.name.encode(), set())
    if len(expected) != 1 or path.is_symlink() or not path.is_file():
        return False
    return sha256_file(path) in expected


def restore_cache(cache, lists_dir, archives_dir):
    if cache.is_symlink():
        print("apt_install: rejected symlink for cache directory", file=sys.stderr)
        return 1
    archives = sorted(cache.glob("*.deb"))
    list_cache = cache / "lists"
    files = {path.name: path for path in sorted(list_cache.iterdir())} if list_cache.is_dir() else {}
    good_archives = []
    restored_lists = 0
    rejected_lists = False
    if files:
        try:
            if list_cache.is_symlink() or any(path.is_symlink() or not path.is_file() for path in files.values()):
                raise ValueError("non-regular cached list")
            digests = verify_lists(files, archives)
            subprocess.run(["sudo", "install", "-m", "0644", "-p", "--", *files.values(),
                            f"{lists_dir}/"], check=True)
            restored_lists = len(files)
        except (OSError, ValueError, KeyError, IndexError, OverflowError, subprocess.SubprocessError) as error:
            print(f"apt_install: rejected cached package lists ({error}); refreshing package lists", file=sys.stderr)
            rejected_lists = True
        else:
            for path in archives:
                try:
                    if verify_archive(path, digests):
                        good_archives.append(path)
                except OSError:
                    continue
    dropped = len(archives) - len(good_archives)
    if dropped:
        print(f"apt_install: dropped {dropped} unverified cached archives", file=sys.stderr)
    restored_archives = 0
    if good_archives:
        if subprocess.run(["sudo", "install", "-m", "0644", "--", *good_archives,
                           f"{archives_dir}/"], check=False).returncode == 0:
            restored_archives = len(good_archives)
        else:
            print("apt_install: could not restore cached archives; continuing with apt", file=sys.stderr)
    print(f"apt_install: restored package lists ({restored_lists} files)")
    print(f"apt_install: restored {restored_archives} cached .deb file(s)")
    return int(rejected_lists)


if __name__ == "__main__":
    sys.exit(restore_cache(*map(pathlib.Path, sys.argv[1:])))
