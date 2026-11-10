---
kind: fix
surfaces: [ci]
issues: []
---
Detail: The macOS CI and nightly soak jobs run on the `macos-15` runner image. GitHub no longer supports `macos-14` ("The configuration 'macos-14-arm64-us-default' is not supported"), so the required `macos-test`, `macos-msrv` and `changed-test-stress-macos` checks were cancelled within one second on every pull request. The job names stay the same, so branch protection is unchanged.
