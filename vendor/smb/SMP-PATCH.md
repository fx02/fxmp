# Local patch

Unmodified `smb` 0.12.1 from crates.io (MIT; the crate package has no licence
file, so `LICENSE` here is copied from `LICENSE.md` in the upstream repo
https://github.com/afiffon/smb-rs) except for:

- `src/resource/directory.rs` (`iter_stream`, `QueryDirectoryStream::poll_next`):
  `notify_fetch_next.notify_waiters()` replaced by `notify_fetch_next.notify_one()`,
  plus a comment;
- packaging leftovers removed: `Cargo.toml.orig`, `.cargo_vcs_info.json`,
  `.cargo-ok`, `Cargo.lock`.

Why: the stream's background fetch loop sends a batch of entries, then waits on
`notified().await` before querying the next page. `poll_next` signals "batch
consumed" with `notify_waiters()`, which wakes only tasks already waiting and
stores no permit. When the consumer drains the batch before the fetch loop
reaches `notified().await` (common for small folders), the wakeup is lost: the
loop never sends the final query that returns `STATUS_NO_MORE_FILES`, and the
stream hangs until the operation timeout. `notify_one()` stores a permit, so the
loop proceeds either way. Measured against the real Samba server (full walk,
~59 400 files): `Timeout` after 130 s without the patch, OK in ~38 s with it.

When upgrading `smb`, re-copy the matching version and re-apply the change (or
drop the patch if upstream fixed the stream).
