# Vendored: xattr 0.2.3 (one-line patch)

Copied verbatim from crates.io (`xattr 0.2.3`, dual MIT/Apache-2.0, kept in
`LICENSE-MIT` and `LICENSE-APACHE`) with exactly one change, in
`src/sys/mod.rs`:

```rust
// upstream
pub const ENOATTR: ::libc::c_int = ::libc::ENOATTR;
// here
#[cfg(target_os = "linux")]
pub const ENOATTR: ::libc::c_int = ::libc::ENODATA;
#[cfg(not(target_os = "linux"))]
pub const ENOATTR: ::libc::c_int = ::libc::ENOATTR;
```

Why: `libc` no longer exports `ENOATTR` on Linux (the Linux name for "no such
attribute" is `ENODATA`), so this 2019 crate fails to compile against the
`libc 0.2.191` this workspace resolves. We need it because

    gpui 0.2.2 → gpui_http_client 0.2.2 → zed-async-tar 0.5.0-zed → xattr 0.2

and although `xattr` is an *optional* dependency of `zed-async-tar`, it is
enabled by that crate's `default` feature, which a downstream workspace cannot
turn off. Wiring it in is one line in the root `Cargo.toml`:

```toml
[patch.crates-io]
xattr = { path = "vendor/xattr" }
```

Remove this directory and the patch when `gpui` moves to a `zed-async-tar` that
uses `xattr 1.x`.
