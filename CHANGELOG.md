# Changelog

All notable changes to this project will be documented in this file.

## [0.3.0] - 2026-11-24

### 🚀 Features

- Recall earlier work when an agent edits a file or reruns a check ([#3](https://github.com/cesarferreira/mem-in-black/issues/3))
- Add a task benchmark for memory against no memory ([#4](https://github.com/cesarferreira/mem-in-black/issues/4))
- Explain a line or file with mib why ([#5](https://github.com/cesarferreira/mem-in-black/issues/5))
- Rank recall by whether earlier deliveries were used ([#6](https://github.com/cesarferreira/mem-in-black/issues/6))
- Remember abandoned, reverted and disputed approaches and recall them first when a file is edited again ([#7](https://github.com/cesarferreira/mem-in-black/issues/7))
- Condense repeated work into project facts ([#8](https://github.com/cesarferreira/mem-in-black/issues/8))

### 🐛 Bug Fixes

- Address review findings in dead ends, judging, facts and mib why ([#9](https://github.com/cesarferreira/mem-in-black/issues/9))

### 📚 Documentation

- Plan point-of-action memory ([#2](https://github.com/cesarferreira/mem-in-black/issues/2))
- Add screenshot and crates.io badges to README header
- Document installing from crates.io

### ⚙️ Miscellaneous Tasks

- Publish mem-in-black to crates.io
## [0.2.0] - 2026-11-21

### 🚀 Features

- Download through the GitHub CLI when a plain download fails
- Write summary titles and outcomes with Claude Haiku
- Scope handoffs to the current Git branch and worktree

### 📚 Documentation

- Explain installing from the private repository
- Drop the private repository install note
- Highlight cross-agent handoffs and recall costs

### 🧪 Testing

- Keep capture fixtures independent of Claude login
## [0.1.1] - 2026-11-20

### 💼 Other

- Stop building release binaries for Intel Macs
## [0.1.0] - 2026-11-20

### 🚀 Features

- Show recent memory when sessions start
- Add a local startup preview for Pi
- Install MCP for OpenCode, Pi, and Oh My Pi
- Distill each turn into a structured local summary
- Capture reads, searches, web lookups and edit sizes
- Forget records and supersede outdated ones
- Add a retrieval benchmark over claude-mem history
- Match word forms in search with stemming
- Add local semantic search with calibrated recall
- Show usage stats and estimated token savings
- Show download progress when enabling embeddings
- Unload the embedding model when idle
- Halve embedding memory and cap worker CPU
- Install into T3 Code's Codex and Claude Code providers
- [**breaking**] Rename QuietMem to Mem in Black (mib)
- Add a live project-colored memory viewer
- Show agent badges in the memory viewer
- Capture OpenCode, Pi, and Oh My Pi sessions
- Recall verified project work across agents
- Render Markdown in the memory viewer
- Rename the app to mem-in-black
- Print the viewer URL from mib open
- Make work summaries accurate and readable
- Show which recalled files changed and how often recall is used
- Show work outcomes in the startup preview
- Tell agents when to search memory
- Install release binaries with a curl script

### 🐛 Bug Fixes

- Support placing the memory database on local disk
- Recall past outcomes instead of repeating old prompts
- Import databases from current claude-mem releases
- Return results for natural-language searches
- Ignore harness notifications in recall and capture
- Keep semantic queries fast while the worker backfills
- Stop automatic recall from injecting unrelated records
- Close Markdown that shortened records leave open
- Start the summary rebuild before the viewer serves

### 💼 Other

- Build local Rust memory service for coding agents

### 🚜 Refactor

- Drop support for the quietmem and meminblack names

### 📚 Documentation

- Use Rust CLI template for README
- Make local-only defaults explicit
- Note semantic search in the parity summary
- Summarize benefits and costs at a glance
- Tighten the README and document T3 Code

### 🎨 Styling

- Format the viewer launch test

### 🧪 Testing

- Make the viewer launch test independent of the umask

### ⚙️ Miscellaneous Tasks

- Adopt standard Rust CLI scaffold
