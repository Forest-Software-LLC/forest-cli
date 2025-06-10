import { success, info } from '../utils/logger.js';
import { makeRequest } from '../utils/httpHelper.js';
import { readFileSync, writeFileSync, existsSync, createReadStream } from 'fs';
import { create } from 'tar';
import path from 'path';
import ignore from 'ignore';
import FormData from 'form-data';
import { PassThrough } from 'stream';
import { getStreamAsBuffer } from 'get-stream';
import { SingleBar, Presets } from 'cli-progress';
import { Message } from '../utils/logger.js';

import { Readable } from 'stream';

import got from "got"

export function loadForestIgnore(directory : string) {
  const ig = ignore();

  const ignorePath = path.join(directory, '.forestignore');
  if (!existsSync(ignorePath)) {
    // If there’s no .forestignore, return an “ignore()” that matches nothing
    return ig;
  }

  const content = readFileSync(ignorePath, 'utf-8');
  // Add each pattern (e.g. "dist/", "node_modules/", etc.)
  ig.add(content);

  return ig;
}

async function createTarballBuffer(directory: string): Promise<Buffer> {
  const ig = loadForestIgnore(directory);

  // We'll pipe tar.create(...) into a PassThrough, then buffer it
  const tarStream = new PassThrough();

  // Filter out "." first, then strip leading "./" for ignore()
  const filterFn = (relativePath: string) => {
    if (relativePath === '.') return true;
    const trimmed = relativePath.startsWith('./')
      ? relativePath.slice(2)
      : relativePath;
    return !ig.ignores(trimmed);
  };

  // Start tarball generation (do NOT await, since it returns a stream)
  create(
      {
        gzip: true,
        cwd: directory,
        filter: filterFn,
      },
      ['.']
    )
    .on('error', (err) => tarStream.destroy(err as Error)) // Handle errors by destroying the stream
    .pipe(tarStream);

  // Collect the entire tarball into a Buffer (only ~10 MB max)
  const buffer = await getStreamAsBuffer(tarStream);
  return buffer;
}

export async function publishCommand() {
    let msg = new Message("Publishing package...");

    if (!existsSync('forest.json')) {
      msg.fail('No forest.json found in the current directory. Please run `forest init` to create a new package.');
      return;
    }

    const packageInfo = JSON.parse(readFileSync('forest.json', 'utf-8'));

    packageInfo.public = true
    const tarStream = await createTarballBuffer(process.cwd());

    const formData = new FormData();
    console.log(JSON.stringify(packageInfo));

    formData.append('file', tarStream, { filename: 'package.tgz' }); 
    formData.append('metadata', JSON.stringify(packageInfo));

    makeRequest('v1/package/upload', {
        method : "POST",
        headers: {
            ...formData.getHeaders(),
        },
        body : formData
    }).then(async (response) => {
        console.log('Response status:', response);
        msg.success('Package uploaded successfully!');
    }).catch((error) => {        
        msg.fail(`Failed to upload package: ${error.message}`);
    })
}
