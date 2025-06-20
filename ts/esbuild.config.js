// esbuild.config.js
import { build } from 'esbuild';

build({
  entryPoints: ['dist/cli.js'],    // your compiled ESM output
  bundle: true,
  platform: 'node',
  target: 'node18',
  format: 'cjs',
  outfile: 'dist/bundled.js',
  external: [],                    // list any truly-externals here
}).catch(() => process.exit(1));
