# cargo-apply

Builds or tests packages by name from crates.io, saving timing information and other results.

```
cargo-apply [options] <package-name>...
```

Each package-name should either be something like
`regex`, to select the latest version, or `regex=1.0`, to select a
specific version. If the special package-name "\*" is used, we will
test all packages. (Use `'*'` to prevent your shell from expanding
wildcards.)

**WARNING:** Building or testing packages from crates.io involves executing
arbitary code! Be wary.
