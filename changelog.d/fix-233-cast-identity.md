---
kind: fix
surfaces: []
---
Ship a `NoDisplay=true` desktop entry and rename the Linux user unit to
`app-dormant.service` so portal casting indicators can label active
wear-sampling sessions as `dormant`. Users upgrading from `dormant.service`
must disable the old unit and enable the new one.
