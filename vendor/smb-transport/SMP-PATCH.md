# Local patch

Unmodified `smb-transport` 0.12.1 from crates.io (MIT; the crate package has
no licence file, so `LICENSE` here is copied from `LICENSE.md` in the upstream
repo https://github.com/afiffon/smb-rs) except for:

- `src/tcp/transport.rs` (`do_connect`): `socket.set_nodelay(true)` plus a comment;
- packaging leftovers removed: `Cargo.toml.orig`, `.cargo_vcs_info.json`, `Cargo.lock`.

Why: the transport writes each request as several small writes (4-byte header,
then the body). With Nagle's algorithm on, the second write waits for the
server's delayed ACK, adding ~40-80 ms to every request. Measured against the
real Windows server: walking ~10 900 files took 255 s without the patch, 70 s
with it.

When upgrading `smb`, re-copy the matching `smb-transport` version and re-apply
the line (or drop the patch if upstream sets TCP_NODELAY itself).
