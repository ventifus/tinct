import * as vscode from 'vscode';
import {
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
} from 'vscode-languageclient/node';

let client: LanguageClient | undefined;

export function activate(context: vscode.ExtensionContext) {
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

  return {
    extendMarkdownIt(md: any) {
      const orig: ((str: string, lang: string) => string) | undefined =
        md.options.highlight;
      md.options.highlight = function (str: string, lang: string): string {
        const l = lang.trim().toLowerCase();
        if (l === 'llt' || l === 'tinct') {
          return (
            '<pre class="hljs"><code class="hljs language-llt">' +
            highlightTinct(str) +
            '</code></pre>'
          );
        }
        return orig ? orig.call(this, str, lang) : '';
      };
      return md;
    },
  };
}

export function deactivate(): Thenable<void> | undefined {
  return client?.stop();
}

function esc(s: string): string {
  return s
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;');
}

function span(cls: string, text: string): string {
  return `<span class="hljs-${cls}">${esc(text)}</span>`;
}

function highlightTinct(code: string): string {
  let result = '';
  let i = 0;
  let depth = 0;
  const n = code.length;

  while (i < n) {
    const rest = code.slice(i);
    let m: RegExpMatchArray | null;

    // Comment: # to end of line
    m = rest.match(/^(#[^\n]*)/);
    if (m) { result += span('comment', m[1]); i += m[1].length; continue; }

    // String (regular or interpolated)
    m = rest.match(/^(i?"(?:[^"\\]|\\.)*")/);
    if (m) { result += span('string', m[1]); i += m[1].length; continue; }

    // Document separator ---
    m = rest.match(/^(---)/);
    if (m && (i + 3 >= n || /\s/.test(code[i + 3]))) {
      result += span('meta', m[1]); i += 3; continue;
    }

    // Pipeline ref %name or bare %
    m = rest.match(/^(%[a-zA-Z_][a-zA-Z0-9_-]*|%)/);
    if (m) { result += span('variable', m[1]); i += m[1].length; continue; }

    // Variable $name or $$
    m = rest.match(/^(\$\$|\$[a-zA-Z_][a-zA-Z0-9_-]*)/);
    if (m) { result += span('variable', m[1]); i += m[1].length; continue; }

    // Type annotation @Type
    m = rest.match(/^(@[A-Za-z][A-Za-z0-9]*(?:\[.*?\])?)/);
    if (m) { result += span('type', m[1]); i += m[1].length; continue; }

    // Number
    m = rest.match(/^(-?\d+(?:\.\d+)?)\b/);
    if (m) { result += span('number', m[1]); i += m[1].length; continue; }

    // Identifier: keyword, literal, or plain word
    m = rest.match(/^([a-zA-Z_][a-zA-Z0-9_-]*)/);
    if (m) {
      const word = m[1];
      if (word === 'fn' || word === 'call' || word === 'type') {
        result += span('keyword', word);
      } else if (word === 'true' || word === 'false') {
        result += span('literal', word);
      } else {
        result += esc(word);
      }
      i += word.length;
      continue;
    }

    // Pipe operator
    if (code[i] === '|') { result += span('operator', '|'); i++; continue; }

    // Opening bracket — color at current depth, then deepen
    if (code[i] === '[') {
      result += `<span class="llt-bracket-${depth % 3}">[</span>`;
      depth++;
      i++;
      continue;
    }

    // Closing bracket — shallow first, then color at new depth
    if (code[i] === ']') {
      depth = Math.max(0, depth - 1);
      result += `<span class="llt-bracket-${depth % 3}">]</span>`;
      i++;
      continue;
    }

    // Colon (key separator)
    if (code[i] === ':') { result += span('punctuation', ':'); i++; continue; }

    result += esc(code[i]);
    i++;
  }

  return result;
}
