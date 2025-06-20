// src/utils/fetchAndExtract.ts
import got from 'got';
import { extract } from 'tar';
import type { SingleBar } from 'cli-progress';
import type { Progress } from 'got';

/**
 * Download a .tgz from the given URL and extract it into outDir.
 *
 * @param url     Full URL to the .tgz (e.g. your R2 bucket object URL)
 * @param outDir  Local directory to extract into (will be created if needed)
 */
export default async function fetchAndExtract(url: string, outDir: string, bar : SingleBar): Promise<void> {

  // 2) Create a promise that resolves when extraction finishes
  await new Promise<void>((resolve, reject) => {
    // tar.x creates an Extract stream
    const extractor = extract({
      cwd: outDir,
      strip: 1,     // remove leading folder in the archive, if any
    });

    // 3) Stream the .tgz from the URL straight into the extractor
    const stream = got.stream(url)

    stream.on('downloadProgress', (progress: Progress) => {
      if (progress.total) {
        bar.update(progress.transferred / progress.total * 100, {
          transferred: progress.transferred,
          total: progress.total,
        });
      }
    });

    stream.on('error', reject)
      .pipe(extractor)
      .on('error', reject)
      .on('finish', resolve);
  });
}
