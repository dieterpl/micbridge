# Third-party notices

micbridge itself is Apache-2.0. Its dependency tree was audited at v1.0.0:
all 369 third-party packages are permissively licensed, and the two that offer a
copyleft option among several (`r-efi`, `self_cell`) are taken under their
permissive alternative.

Two obligations travel with the **binaries** rather than with the source, because
the fonts are compiled into the GUI executable.

## Embedded fonts (`micbridge-gui` only)

`eframe`'s `default_fonts` feature embeds, via `epaint_default_fonts`:

| Font | Licence |
|------|---------|
| Ubuntu Sans, Ubuntu Sans Mono | Ubuntu Font Licence 1.0 |
| Noto Emoji, Emoji Icon Font | SIL Open Font License 1.1 |

Both permit redistribution, including inside a compiled binary, provided the
licence notice accompanies the distribution. This file is that notice, and it ships
in every release archive alongside `LICENSE-APACHE`.

Full texts: <https://ubuntu.com/legal/font-licence> and
<https://openfontlicense.org>.

The CLI (`micbridge`) embeds no fonts and carries neither obligation.

## Build-time only

`winresource` (MIT) and its `version_check` (MIT/Apache-2.0) run in `build.rs` to
compile the icon into the Windows executables. Nothing from either is linked into
a binary, so they carry no distribution obligation and appear here only so the
dependency list and the lockfile agree.

## Regenerating this audit

```sh
cargo metadata --format-version 1 --all-features \
  | python3 -c 'import json,sys,collections
c=collections.Counter(p.get("license") or "UNKNOWN" for p in json.load(sys.stdin)["packages"])
[print(f"{n:5d}  {l}") for l,n in c.most_common()]'
```
