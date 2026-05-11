const esbuild = require('esbuild');

esbuild.build({
  entryPoints: ['src/extension.ts'],
  bundle: true,
  external: ['vscode'],
  format: 'cjs',
  platform: 'node',
  outfile: 'out/extension.js',
  sourcemap: true,
}).catch(() => process.exit(1));
