/**
 * Builds the demo two ways.
 *
 *   dist/index.html            + app.js + wasm/  — what the site serves
 *   dist/autonomi-webrtc.html  one file, nothing external — what you can share
 *
 * The single-file build is the more interesting artefact: it loads nothing over
 * the network at all, so it has a stable hash anyone can check, it runs from a
 * `file://` URL, and it can be handed around like a document. That it works at
 * all is part of the point — a page that needs no server of its own to reach
 * the network.
 */

import * as esbuild from 'esbuild'
import fs from 'node:fs/promises'
import path from 'node:path'
import { gzipSync } from 'node:zlib'

const outdir = 'dist'
const WASM = 'src/wasm/autonomi_wasm_client_bg.wasm'
const SINGLE_FILE = 'autonomi-webrtc.html'

await fs.rm(outdir, { recursive: true, force: true })
await fs.mkdir(outdir, { recursive: true })

const html = await fs.readFile('index.html', 'utf8')

// --- multi-file build -------------------------------------------------------

const multi = await esbuild.build({
  entryPoints: ['src/app.js'],
  bundle: true,
  format: 'esm',
  target: 'es2022',
  minify: true,
  sourcemap: true,
  outfile: path.join(outdir, 'app.js'),
  // The wasm-bindgen glue fetches this next to itself at runtime.
  external: ['./autonomi_wasm_client_bg.wasm'],
  metafile: true
})

await fs.writeFile(path.join(outdir, 'index.html'), html)
await fs.cp('src/wasm', path.join(outdir, 'wasm'), { recursive: true })

// --- single-file build ------------------------------------------------------

const inline = await esbuild.build({
  entryPoints: ['src/app.js'],
  bundle: true,
  format: 'esm',
  target: 'es2022',
  minify: true,
  write: false,
  external: ['./autonomi_wasm_client_bg.wasm']
})

const wasm = await fs.readFile(WASM)
const packed = gzipSync(wasm, { level: 9 }).toString('base64')

// A literal `</script` anywhere inside a script element would close it early.
const escape = (text) => text.replaceAll('</script', '<\\/script')

// Replaced via a function, not a string: minified JS contains `$&` and `$'`
// sequences, which `String.replace` would expand as match references and splice
// parts of the document back into the output.
const standalone = html.replace(
  '<script type="module" src="./app.js"></script>',
  () =>
    `<script>window.__AUTONOMI_WASM_GZ__="${packed}"</script>\n` +
    `<script type="module">${escape(inline.outputFiles[0].text)}</script>`
)

if (standalone === html) {
  throw new Error('single-file build: script tag not found in index.html')
}
await fs.writeFile(path.join(outdir, SINGLE_FILE), standalone)

// --- report -----------------------------------------------------------------

const kib = (bytes) => `${(bytes / 1024).toFixed(0)} KiB`
const bundled = Object.values(multi.metafile.outputs)
  .reduce((total, output) => total + output.bytes, 0)

console.log(`bundle:      ${kib(bundled)}`)
console.log(`wasm:        ${kib(wasm.length)} → ${kib(packed.length)} gzipped + Base64`)
console.log(`single file: ${kib(Buffer.byteLength(standalone))}  (${SINGLE_FILE})`)
console.log(`\nServe with:  npm run serve   (then open http://localhost:8080)`)
