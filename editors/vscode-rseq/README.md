# rseq VS Code Extension

This extension adds `.rseq` language support:

- automatic language activation for `*.rseq`;
- TextMate syntax highlighting;
- snippets for common rseq constructs;
- automatic `rseq-lsp` startup for diagnostics, completions, and hover.

## Run From This Repository

Open this repository in VS Code, then launch an Extension Development Host from
`editors/vscode-rseq`:

```bash
cd editors/vscode-rseq
npm install
```

In VS Code, open `editors/vscode-rseq`, press `F5`, and open a `.rseq` file in
the new window. Because the workspace contains `crates/rseq-lsp`, the extension
will start:

```bash
cargo run -q -p rseq-lsp --
```

## Settings

```json
{
  "rseq.lsp.command": "auto",
  "rseq.lsp.chips": ["qmi8660.yaml"]
}
```

`auto` uses `cargo run -q -p rseq-lsp --` inside this repository. Outside this
repository it tries to run `rseq-lsp` from `PATH`; install it with:

```bash
cargo install --path crates/rseq-lsp
```

Chip YAML files referenced by `chip!("...")` are loaded automatically, so
`rseq.lsp.chips` is only needed for scratch files that omit `chip!`.

## Package

After `npm install`, package with `vsce` if desired:

```bash
npx @vscode/vsce package
code --install-extension rseq-vscode-0.1.0.vsix
```
