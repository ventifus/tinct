# tinct-lsp

Tinct language server for Claude Code, providing diagnostics, hover types, and go-to-definition for `.llt` files.

## Supported Extensions

`.llt`, `.tinct`

## Installation

Build and install the `tinct` binary:

```bash
cargo install --path /path/to/tinct
```

Make sure `~/.cargo/bin` is in your PATH, then install the plugin:

```
/plugin install /path/to/tinct/integrations/claude
```

## Development

For development, point Claude Code at the debug binary by overriding `command` in a local copy of `plugin.json`:

```json
"command": "/path/to/tinct/target/debug/tinct"
```
