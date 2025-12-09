# reminder

Multi-account GitHub triage desktop app built with eframe/egui. It surfaces review requests, mentions, recent reviews, and notifications for several identities so you can sweep queues quickly without hopping profiles.

## Features
- Track multiple GitHub accounts; each gets its own dashboard card with manual and auto-refresh (every ~180s) so long-running network work stays off the UI thread.
- Notification buckets: review requests, mentions, recent reviews, and everything else. Section headers display live counts for `unseen` and `updated` items within the bucket, matching the current filter.
- Visual cues: unread rows use normal text; seen rows fade to a weaker palette; threads updated after `last_read_at` get a warning color plus an inline `Updated` badge so churn is obvious even if GitHub marked them read.
- Inline search filter per account matches repository, subject, or reason fields.
- Links open the underlying GitHub issue/PR page (subject URLs are normalized from `/pulls/` to `/pull/`).

## Setup
- Requires Rust (edition 2024) and a GitHub Personal Access Token per account with `notifications` and repo read scope.
- Tokens are stored in plaintext at `~/.reminder/accounts.json`; secure storage is a TODO.

## Running
```bash
cargo run --release
```
Use the left side panel to add an account (login + PAT), then expand a card to view tables. Manual refresh is available per account; auto-refresh runs when data is older than the configured interval and no fetch is active.

## Developing
- Format and lint: `cargo fmt` and `cargo clippy --all-targets --all-features -D warnings`.
- Check builds quickly: `cargo check`.
- UI profiling: `cargo run --release`.

## Known limitations
- "Done" actions are intentionally disabled until GitHub exposes filtering that can hide already-archived notifications.
- Secure token storage is not yet implemented; avoid sharing hosts where plaintext PATs would be risky.
