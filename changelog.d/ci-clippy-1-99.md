---
kind: improvement
surfaces: [ci]
issues: []
---
Detail: The workspace passes clippy 1.99 with pedantic lints. async-trait moves to 0.1.92, whose expansion no longer trips `double_must_use`. Test assertions of the form `assert!(x.is_empty())` become `assert_eq!`/`assert_ne!` comparisons that print the value on failure (`assert_is_empty`), the state publisher reborrows its pinned transport future directly (`mut_mut`), and a doctor test drops an assertion that could never fail.
