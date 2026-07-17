# dormant-bin

Arch Linux AUR package for [dormant](https://github.com/legion-works/dormant) —
OLED screen blanking daemon that wakes displays when presence sensors detect
you and blanks them when the room is empty.

This is a **-bin** package: it installs the pre-built x86_64 release binaries
from the upstream GitHub releases. No compilation needed.

## Publishing to AUR

Publishing requires the maintainer's AUR SSH key, which is out of scope for
this repository. The package lives here so the PKGBUILD can be versioned
alongside the source.

## Updating for a new release

```bash
# Bump pkgver in PKGBUILD
sed -i 's/pkgver=OLD/pkgver=NEW/' PKGBUILD

# Download the new tarballs and update sha256sums
updpkgsums

# Regenerate .SRCINFO
makepkg --printsrcinfo > .SRCINFO

# Commit and push to the dormant repo, then push to AUR:
# git -C /path/to/aur-dormant-bin pull --rebase
# cp PKGBUILD .SRCINFO /path/to/aur-dormant-bin/
# cd /path/to/aur-dormant-bin && makepkg --printsrcinfo > .SRCINFO
# git commit -am "dormant-bin: bump to NEW" && git push
```
