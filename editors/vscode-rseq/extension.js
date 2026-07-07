const fs = require("fs");
const path = require("path");
const vscode = require("vscode");
const { LanguageClient } = require("vscode-languageclient/node");

let client;

async function activate(context) {
  context.subscriptions.push(
    vscode.commands.registerCommand("rseq.restartLanguageServer", async () => {
      await restartClient(context);
    }),
  );

  context.subscriptions.push(
    vscode.workspace.onDidChangeConfiguration(async (event) => {
      if (event.affectsConfiguration("rseq.lsp")) {
        await restartClient(context);
      }
    }),
  );

  await startClient(context);
}

async function deactivate() {
  await stopClient();
}

async function restartClient(context) {
  await stopClient();
  await startClient(context);
}

async function stopClient() {
  if (!client) {
    return;
  }

  const existing = client;
  client = undefined;
  await existing.stop();
}

async function startClient(context) {
  const workspaceFolder = vscode.workspace.workspaceFolders?.[0];
  const resolved = resolveServerConfig(workspaceFolder);
  const serverOptions = {
    command: resolved.command,
    args: resolved.args,
    options: {
      cwd: resolved.cwd,
      env: process.env,
    },
  };
  const clientOptions = {
    documentSelector: [
      { scheme: "file", language: "rseq" },
      { scheme: "untitled", language: "rseq" },
    ],
    initializationOptions: {
      chips: resolved.chips,
    },
    outputChannelName: "rseq Language Server",
  };

  client = new LanguageClient(
    "rseq-lsp",
    "rseq Language Server",
    serverOptions,
    clientOptions,
  );
  context.subscriptions.push(client);

  try {
    await client.start();
    vscode.window.setStatusBarMessage(
      `rseq-lsp: ${resolved.command} ${resolved.args.join(" ")}`,
      3000,
    );
  } catch (error) {
    client = undefined;
    vscode.window.showErrorMessage(
      `Failed to start rseq-lsp: ${error instanceof Error ? error.message : String(error)}`,
    );
  }
}

function resolveServerConfig(workspaceFolder) {
  const cfg = vscode.workspace.getConfiguration("rseq");
  const workspacePath = workspaceFolder?.uri.fsPath;
  const cwd = resolveCwd(cfg.get("lsp.cwd", ""), workspacePath);
  const chips = cfg.get("lsp.chips", []);
  const commandSetting = cfg.get("lsp.command", "auto").trim();

  let command;
  let args;
  if (!commandSetting || commandSetting === "auto") {
    if (isRseqWorkspace(cwd)) {
      command = "cargo";
      args = ["run", "-q", "-p", "rseq-lsp", "--"];
    } else {
      command = "rseq-lsp";
      args = [];
    }
  } else {
    command = commandSetting;
    args = cfg.get("lsp.args", []);
  }

  for (const chip of chips) {
    args.push("--chip", chip);
  }

  return { command, args, cwd, chips };
}

function resolveCwd(setting, workspacePath) {
  if (setting && setting.trim()) {
    if (workspacePath) {
      return setting.replaceAll("${workspaceFolder}", workspacePath);
    }
    return setting;
  }

  return workspacePath || process.cwd();
}

function isRseqWorkspace(cwd) {
  return (
    fs.existsSync(path.join(cwd, "Cargo.toml")) &&
    fs.existsSync(path.join(cwd, "crates", "rseq-lsp", "Cargo.toml"))
  );
}

module.exports = {
  activate,
  deactivate,
  resolveServerConfig,
};
