import * as vscode from 'vscode';
import {
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
} from 'vscode-languageclient/node';

let client: LanguageClient | undefined;

export function activate(context: vscode.ExtensionContext): void {
  const config = vscode.workspace.getConfiguration('tinct');
  const serverPath = config.get<string>('serverPath', 'tinct');

  const serverOptions: ServerOptions = {
    command: serverPath,
    args: ['lsp'],
  };

  const clientOptions: LanguageClientOptions = {
    documentSelector: [{ scheme: 'file', language: 'llt' }],
    synchronize: {
      fileEvents: vscode.workspace.createFileSystemWatcher('**/*.llt'),
    },
  };

  client = new LanguageClient(
    'tinct',
    'Tinct Language Server',
    serverOptions,
    clientOptions,
  );

  client.start();
  context.subscriptions.push(client);
}

export function deactivate(): Thenable<void> | undefined {
  return client?.stop();
}
