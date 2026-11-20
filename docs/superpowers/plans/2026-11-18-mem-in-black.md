# Mem in Black Implementation Plan

The design is in [Mem in Black](../specs/2026-11-18-mem-in-black.md). Use test-first integration checks against temporary configuration and storage. Execute sequentially in the new standalone repository.

- [x] Define the CLI and build hook fixtures for a Claude-to-Codex handoff, an unrelated prompt, competing worktrees and repeated events. Run the fixtures against the empty binary. Implement the SQLite store, hook parsing, local observations and prompt-scoped recall. Repeat the fixtures.
- [x] Exercise MCP initialization, tool listing, search and detail over stdio. Add the Rust MCP SDK and check that tool results carry historical data and source IDs.
- [x] Test isolated installer runs against seeded host configuration. Register both host hooks and the MCP server while retaining the existing configuration. Add an idempotent lazy-start worker, loopback viewer and a status command.
- [x] Exercise import, opt-in compression and restart behavior with fixture data and a local HTTP provider. Document setup, data paths, and the parity matrix. Run formatting, tests, lint and a release build.
