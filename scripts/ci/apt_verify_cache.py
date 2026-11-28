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
PUBLIC_KEY = b"-----BEGIN PGP " + b"PUBLIC KEY BLOCK-----"
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
        signed_by = b""
        if options:
            pairs = (option.partition(b"=") for option in options[1].split())
            signed_by = {name.lower(): value for name, _, value in pairs}.get(b"signed-by", b"")
    if signed_by:
        if signed_by.strip().startswith(PUBLIC_KEY):
            lines = (line.strip() for line in signed_by.strip().splitlines())
            key = b"\n".join(b"" if line == b"." else line for line in lines) + b"\n"
            return ((f"inline Signed-By in {entry}", key),)
        rings = signed_by.replace(b",", b" ").decode().split()
        if not all(pathlib.Path(ring).is_absolute() and ring.endswith((".gpg", ".asc")) for ring in rings):
            raise ValueError(f"unsupported Signed-By in {entry}")
        return tuple((ring, None) for ring in sorted(rings))
    raise ValueError("source has no Signed-By")


def release_hashes(path, suite, keyrings):
    with path.open("rb") as stream:
        if stream.readline(len(SIGNED_MESSAGE) + 1) != SIGNED_MESSAGE:
            raise ValueError(f"not a clear-signed Release: {path.name}")
    with tempfile.TemporaryDirectory() as directory:
        command = ["gpgv", "--output", "-"]
        for index, (label, key) in enumerate(keyrings):
            ring = label
            if key is not None or label.endswith(".asc"):
                ring = str(pathlib.Path(directory) / f"key-{index}.gpg")
                if key is None:
                    key = pathlib.Path(label).read_bytes()
                result = subprocess.run([
                    "gpg", "--no-options", "--homedir", directory, "--batch", "--dearmor", "--output", ring,
                ], input=key, capture_output=True, check=False)
                if result.returncode != 0:
                    raise ValueError(f"cannot dearmor {label}")
            command.extend(["--keyring", ring])
        verified = subprocess.run([*command, "--", path], capture_output=True, check=False)
    if verified.returncode != 0:
        raise ValueError(f"bad signature: {path.name}")
    fields, = paragraphs(verified.stdout.splitlines())
    names = {fields.get(b"suite")}
    if (UBUNTU_KEYRING, None) not in keyrings:
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
    if b"\r\n" in content:
        content = content.replace(b"\r\n", b"\n")
    for match in wanted.finditer(content):
        previous = content.rfind(b"\n\n", 0, match.start())
        start = previous + 2 if previous != -1 else 0
        end = content.find(b"\n\n", match.end())
        fields = next(paragraphs(content[start:None if end == -1 else end].splitlines()))
        package, version, arch, digest = (fields.get(field, b"") for field in
                                          (b"package", b"version", b"architecture", b"sha256"))
        if version and arch:
            name = b"_".join((package, version.replace(b":", b"%3a"), arch)) + b".deb"
            digests.setdefault(name, set()).add(digest.lower() if SHA256.fullmatch(digest) else None)


VERIFY_ERRORS = (OSError, ValueError, KeyError, IndexError, OverflowError, subprocess.SubprocessError)


def verify_source(indexes, signed, decoded):
    representations = {}
    for path, metakey, extension in indexes:
        representations.setdefault(metakey, []).append((path, extension))
    verified = []
    for metakey, candidates in representations.items():
        last_error = "no candidate"
        for path, extension in candidates:
            try:
                if path.is_symlink() or not path.is_file():
                    raise ValueError(f"non-regular cached index: {path.name}")
                content = read_index(path, metakey, extension, signed)
            except VERIFY_ERRORS as error:
                last_error = str(error)
                continue
            if extension and content is not None:
                plain = decoded / path.name.removesuffix(extension)
                plain.write_bytes(content)
                shutil.copystat(path, plain)
                path = plain
            verified.append(path)
            break
        else:
            raise ValueError(f"no authenticated representation of {metakey}: {last_error}")
    return verified


def verify_lists(files, decoded):
    targets, releases = apt_targets()
    groups = {name: [] for name in files if name.endswith("_InRelease")}
    for name, path in files.items():
        extension = next((ext for ext in COMPRESSION if name.endswith(ext)), "")
        if target := targets.get(name.removesuffix(extension)):
            release, metakey = target
            groups.setdefault(release, []).append((path, metakey, extension))
    verified = []
    for release, indexes in groups.items():
        key_label = "none"
        try:
            sources = releases.get(release)
            if not sources:
                raise ValueError("source is not configured")
            key_label = "unresolved; " + ", ".join(sorted({entry for _, entry in sources}))
            suites = {suite for suite, _ in sources}
            rings = {source_keyrings(entry) for _, entry in sources}
            if len(suites) != 1 or len(rings) != 1:
                raise ValueError("ambiguous source configuration")
            keyrings = rings.pop()
            key_label = ", ".join(label for label, _ in keyrings)
            if release not in files:
                raise ValueError("missing InRelease")
            path = files[release]
            if path.is_symlink() or not path.is_file():
                raise ValueError("non-regular InRelease")
            if not indexes:
                raise ValueError("no configured cached indexes")
            signed = release_hashes(path, suites.pop(), keyrings)
            source_indexes = verify_source(indexes, signed, decoded)
        except VERIFY_ERRORS as error:
            print(f"apt_install: rejected {release}: {error} (keyring: {key_label})", file=sys.stderr)
            continue
        print(f"apt_install: verified {release} (keyring: {key_label}): "
              f"signature and {len(source_indexes)} index hash(es) match")
        verified.extend([path, *source_indexes])
    return verified


def live_archive_digests(lists_dir, archives):
    packages = sorted({path.name.split("_", 1)[0].encode() for path in archives})
    if not packages:
        return {}
    wanted = re.compile(rb"(?im)^Package:[ \t]*(?:" + b"|".join(map(re.escape, packages)) + rb")[ \t]*$")
    digests = {}
    for path in lists_dir.iterdir():
        extension = next((ext for ext in COMPRESSION if path.name.endswith(ext)), "")
        if not path.name.removesuffix(extension).endswith("_Packages"):
            continue
        content = (subprocess.check_output([APT_HELPER, "cat-file", path]) if extension else path.read_bytes())
        archive_digests(content, wanted, digests)
    return digests


def verify_archive(path, digests):
    expected = digests.get(path.name.encode(), set())
    if len(expected) != 1 or path.is_symlink() or not path.is_file():
        return False
    return sha256_file(path) in expected


def remove_index_siblings(lists_dir, verified):
    siblings = []
    for path in verified:
        if path.name.endswith("_InRelease"):
            continue
        extension = next((ext for ext in COMPRESSION if path.name.endswith(ext)), "")
        name = path.name.removesuffix(extension)
        for other in ("", *COMPRESSION):
            if other != extension:
                sibling = lists_dir / (name + other)
                if sibling.exists() or sibling.is_symlink():
                    siblings.append(sibling)
    if siblings:
        subprocess.run(["sudo", "rm", "-f", "--", *siblings], check=True)


def restore_cache(cache, lists_dir, archives_dir, manifest, decoded):
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
            if list_cache.is_symlink():
                raise ValueError("symlink for lists directory")
            verified = verify_lists(files, decoded)
            if verified:
                subprocess.run(["sudo", "install", "-m", "0644", "-p", "--", *verified,
                                f"{lists_dir}/"], check=True)
                remove_index_siblings(lists_dir, verified)
                restored_lists = len(verified)
            else:
                rejected_lists = True
        except VERIFY_ERRORS as error:
            print(f"apt_install: could not restore cached package lists: {error}", file=sys.stderr)
            rejected_lists = True
    if restored_lists:
        try:
            digests = live_archive_digests(lists_dir, archives)
        except VERIFY_ERRORS as error:
            print(f"apt_install: could not check live package indexes: {error}", file=sys.stderr)
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
        manifest.write_bytes(b"".join(bytes(archives_dir / path.name) + b"\0" for path in good_archives))
        if subprocess.run(["sudo", "install", "-m", "0644", "--", *good_archives,
                           f"{archives_dir}/"], check=False).returncode == 0:
            restored_archives = len(good_archives)
        else:
            print("apt_install: could not restore cached archives; continuing with apt", file=sys.stderr)
    print(f"apt_install: restored package lists ({restored_lists} files)")
    print(f"apt_install: restored {restored_archives} cached .deb file(s)")
    return int(rejected_lists)


if __name__ == "__main__":
    with tempfile.TemporaryDirectory() as decoded:
        sys.exit(restore_cache(*map(pathlib.Path, sys.argv[1:]), pathlib.Path(decoded)))
