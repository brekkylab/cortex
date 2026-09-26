# Cortex

Cortex lets you run tasks in disposable VMs.

## Requirements

The `mount` feature, on by default, mounts a cortex filesystem on the host through the host's FUSE provider:

| Host  | Provider | Needed to build | Needed to run |
|-------|----------|-----------------|---------------|
| Linux | `/dev/fuse` in the kernel | — | — |
| macOS | [FUSE-T](https://www.fuse-t.org) | ✓ | ✓ |
| Windows | [Dokany](https://github.com/dokan-dev/dokany) | — | ✓ |

If you don't mount on the host, build with `default-features = false` and skip all of this.

### macOS

```sh
brew install --cask fuse-t
```

### Windows

```powershell
winget install --id dokan-dev.Dokany
```

The build links the installed Dokany library when `DokanLibrary2_LibraryPath_x64` is set,
and otherwise builds one from vendored sources. Prefer the installed one: a self-built
library can disagree with the installed driver's version, which fails only at mount time.
