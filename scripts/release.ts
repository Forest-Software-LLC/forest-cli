/* eslint-disable no-console */
import { createHash } from "node:crypto";
import { createReadStream } from "node:fs";
import { readdir, readFile, stat, writeFile } from "node:fs/promises";
import * as path from "node:path";
import { S3Client, PutObjectCommand } from "@aws-sdk/client-s3";

type FileEntry = {
    bin: string;
    filename: string;
    target: string;
    size: number;
    sha256: string;
    key: string;
    url?: string;
};

function req(name: string): string {
    const v = process.env[name];
    if (!v) throw new Error(`Missing env: ${name}`);
    return v;
}

const DIST_DIR = process.env.DIST_DIR || "dist";
const BUCKET = req("R2_BUCKET");
const S3_ENDPOINT = req("S3_ENDPOINT");
const ACCESS_KEY_ID = req("R2_ACCESS_ID");
const SECRET_ACCESS_KEY = req("R2_ACCESS_SECRET");
const TAG = process.env.TAG || req("GITHUB_REF_NAME");              // e.g. v1.2.3
const PREFIX_ROOT = "cli";             // e.g. cli
const PUBLIC_BASE = process.env.CDN_BASE || "";            // optional CDN/domain
const LATEST_ALIAS = process.env.LATEST_ALIAS || "latest";           // folder for "latest/"
const BIN_NAME_HINT = process.env.BIN_NAME || "";                    // optional hint for parsing
const MIRROR_BINARIES_TO_LATEST = (process.env.MIRROR_BINARIES_TO_LATEST || "false").toLowerCase() === "true";

const tagPrefix = path.posix.join(PREFIX_ROOT, TAG);
const latestPrefix = path.posix.join(PREFIX_ROOT, LATEST_ALIAS);

const s3 = new S3Client({
    region: "auto",
    endpoint: S3_ENDPOINT,
    credentials: { accessKeyId: ACCESS_KEY_ID, secretAccessKey: SECRET_ACCESS_KEY },
    forcePathStyle: true
});

function sha256File(filePath: string): Promise<string> {
    return new Promise((resolve, reject) => {
        const hash = createHash("sha256");
        const stream = createReadStream(filePath);
        stream.on("data", (d) => hash.update(d));
        stream.on("error", reject);
        stream.on("end", () => resolve(hash.digest("hex")));
    });
}

function inferNameParts(filename: string, tag: string, hintedBin?: string) {
    const base = filename.replace(/\.exe$/i, "");
    const parts = base.split("-");
    const tagIdx = parts.findIndex((p) => p === tag);
    if (tagIdx === -1) {
        if (hintedBin) {
            const after = base.startsWith(hintedBin + "-") ? base.slice(hintedBin.length + 1) : base;
            return { bin: hintedBin, target: after };
        }
        throw new Error(`Cannot parse tag '${tag}' from '${filename}'. Provide BIN_NAME env to help.`);
    }
    const bin = hintedBin || parts.slice(0, tagIdx).join("-");
    const target = parts.slice(tagIdx + 1).join("-");
    return { bin, target };
}

async function putText(key: string, body: string, contentType = "text/plain", cache = "public, max-age=60") {
    await s3.send(new PutObjectCommand({
        Bucket: BUCKET,
        Key: key,
        Body: body,
        ContentType: contentType,
        CacheControl: cache
    }));
}

async function putFile(key: string, filePath: string, contentType = "application/octet-stream", cache = "public, max-age=31536000, immutable") {
    await s3.send(new PutObjectCommand({
        Bucket: BUCKET,
        Key: key,
        Body: await readFile(filePath),
        ContentType: contentType,
        CacheControl: cache
    }));
}

(async () => {
    const names = await readdir(DIST_DIR);
    const entries: FileEntry[] = [];

    for (const name of names) {
        if (name.endsWith(".sha256") || name.endsWith(".size") || name === "latest.json" || name === "SHA256SUMS") {
            continue;
        }
        const full = path.join(DIST_DIR, name);
        const st = await stat(full);
        if (!st.isFile()) continue;

        const sha = await sha256File(full);
        const size = st.size;

        await writeFile(path.join(DIST_DIR, `${name}.sha256`), `${sha}  ${name}\n`, "utf8");

        const { bin, target } = inferNameParts(name, TAG, BIN_NAME_HINT || undefined);
        const key = path.posix.join(tagPrefix, name);
        const url = PUBLIC_BASE ? `${PUBLIC_BASE}/${key}` : undefined;

        entries.push({ bin, filename: name, target, size, sha256: sha, key, url });
    }

    if (entries.length === 0) {
        throw new Error(`No binaries found in ${DIST_DIR}`);
    }

    const binName = entries[0].bin;

    const shaLines = entries.map(e => `${e.sha256}  ${e.filename}`).join("\n") + "\n";
    await writeFile(path.join(DIST_DIR, "SHA256SUMS"), shaLines, "utf8");

    const manifest = {
        name: binName,
        tag: TAG,
        version: TAG.startsWith("v") ? TAG.slice(1) : TAG,
        released_at: new Date().toISOString(),
        prefix: tagPrefix,
        files: entries.map(e => ({
            target: e.target,
            filename: e.filename,
            size: e.size,
            sha256: e.sha256,
            key: e.key,
            ...(e.url ? { url: e.url } : {})
        }))
    };
    await writeFile(path.join(DIST_DIR, "latest.json"), JSON.stringify(manifest, null, 2), "utf8");

    // Upload binaries + per-file .sha256 to tag/
    for (const e of entries) {
        const p = path.join(DIST_DIR, e.filename);
        await putFile(e.key, p);
        await putText(`${e.key}.sha256`, `${e.sha256}  ${e.filename}\n`);
    }

    // Upload manifests to tag/ and latest/
    await putText(path.posix.join(tagPrefix, "SHA256SUMS"), shaLines);
    await putText(path.posix.join(latestPrefix, "SHA256SUMS"), shaLines);
    await putText(path.posix.join(tagPrefix, "latest.json"), JSON.stringify(manifest), "application/json");
    await putText(path.posix.join(latestPrefix, "latest.json"), JSON.stringify(manifest), "application/json");

    // Optional: mirror binaries to latest/
    if (MIRROR_BINARIES_TO_LATEST) {
        for (const e of entries) {
            const latestKey = path.posix.join(latestPrefix, e.filename);
            await putFile(latestKey, path.join(DIST_DIR, e.filename));
            await putText(`${latestKey}.sha256`, `${e.sha256}  ${e.filename}\n`);
        }
        console.log(`Mirrored binaries to ${latestPrefix}/`);
    }

    console.log(`Uploaded ${entries.length} binaries + checksums; wrote latest.json & SHA256SUMS to tag and latest.`);
})().catch(err => {
    console.error(err);
    process.exit(1);
});
