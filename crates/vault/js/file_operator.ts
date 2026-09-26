/**
 * Drop-in replacement for SwarmStorage `src/lib/file_operator.ts` built on bolt-vault (WASM).
 *
 * Differences from the original:
 * - Files are encrypted (XChaCha20-Poly1305 per chunk). The manifest (name, type, size, shard
 *   list) is encrypted too; only the recipients' secret keys open it.
 * - Real Reed-Solomon erasure coding (default 4 data + 2 parity shards per stripe), so any 2
 *   shards of a stripe may be lost. The original's parity shards were all zeros.
 * - Sizes are padded to buckets (4 KiB … 256 KiB chunks), not to 20 MB per stripe.
 * - Every shard is checked against its CID before use.
 *
 * Build the WASM package (from the Boltchain repository):
 *   cargo build -p bolt-vault --release --target wasm32-unknown-unknown --features wasm
 *   wasm-bindgen --target web --out-dir pkg target/wasm32-unknown-unknown/release/bolt_vault.wasm
 * and import it below (adjust the path).
 */
import init, * as vault from './pkg/bolt_vault';

export interface IUploadCallbacks {
  onProgress?: (percentage: number) => void;
  /** `cid` is the envelope CID: it names the file for all recipients. */
  onSuccess?: (cid: string, info: IVaultUpload) => void;
  onError?: (error: string) => void;
}

export interface IDownloadCallbacks {
  onProgress?: (percentage: number) => void;
  onSuccess?: (file: Blob, filename: string) => void;
  onError?: (error: string) => void;
}

export interface IVaultUpload {
  envelopeCid: string;
  manifestCid: string;
  shardCids: string[];
  /** Keep only if more recipients will be added later (see `addRecipient`); never upload it. */
  contentKey: Uint8Array;
}

let ready: Promise<unknown> | null = null;
const loadVault = () => (ready ??= init());

const uploadBlock = async (bytes: Uint8Array, expectedCid: string): Promise<void> => {
  const form = new FormData();
  form.append('file', new Blob([bytes as unknown as BlobPart]), expectedCid);
  const res = await fetch('/api/v1/file', { method: 'POST', body: form });
  if (!res.ok) throw new Error(`Upload failed: ${res.status}`);
  const cid = (await res.json())?.payload?.hash;
  // The backend must store raw blocks (CIDv1, raw, sha2-256) so the CIDs match the manifest.
  if (cid && cid !== expectedCid) throw new Error(`Backend returned ${cid}, expected ${expectedCid}`);
};

const downloadBlock = async (cid: string): Promise<Uint8Array | null> => {
  try {
    const res = await fetch(`/api/v1/file/${cid}`);
    if (!res.ok) return null;
    const bytes = new Uint8Array(await res.arrayBuffer());
    return vault.cidOf(bytes) === cid ? bytes : null; // a wrong block counts as missing
  } catch {
    return null;
  }
};

/** A new X25519 key pair for receiving files (store the secret key safely). */
export const generateKeyPair = async (): Promise<{ secretKey: Uint8Array; publicKey: Uint8Array }> => {
  await loadVault();
  return vault.generateKeyPair();
};

/** Encrypts, splits and uploads `file`; only `recipients` (32-byte public keys) can read it. */
export const uploadFile = async (file: File, recipients: Uint8Array[], callbacks: IUploadCallbacks) => {
  try {
    await loadVault();
    const data = new Uint8Array(await file.arrayBuffer());
    const sealed = vault.seal(data, file.name, file.type || 'application/octet-stream', recipients);
    const blocks = [...sealed.shards, sealed.manifest, sealed.envelope];
    let done = 0;
    for (const b of blocks) {
      await uploadBlock(b.bytes, b.cid);
      done += 1;
      callbacks.onProgress?.(Math.min((done / blocks.length) * 100, 99));
    }
    callbacks.onProgress?.(100);
    callbacks.onSuccess?.(sealed.envelope.cid, {
      envelopeCid: sealed.envelope.cid,
      manifestCid: sealed.manifest.cid,
      shardCids: sealed.shards.map((s: { cid: string }) => s.cid),
      contentKey: sealed.contentKey,
    });
  } catch (err) {
    callbacks.onError?.(err instanceof Error ? err.message : 'Unknown error');
  }
};

/** Lets one more recipient read an uploaded file; returns the new envelope CID. */
export const addRecipient = async (envelopeCid: string, contentKey: Uint8Array, publicKey: Uint8Array) => {
  await loadVault();
  const envelope = await downloadBlock(envelopeCid);
  if (!envelope) throw new Error('Envelope not found');
  const next = vault.addRecipient(envelope, contentKey, publicKey);
  await uploadBlock(next.bytes, next.cid);
  return next.cid as string;
};

/** Downloads and decrypts a file with a recipient's secret key. */
export const downloadFile = async (envelopeCid: string, secretKey: Uint8Array, callbacks: IDownloadCallbacks) => {
  try {
    await loadVault();
    callbacks.onProgress?.(1);
    const envelope = await downloadBlock(envelopeCid);
    if (!envelope) throw new Error('Envelope not found');
    const { manifestCid, contentKey } = vault.openEnvelope(envelope, secretKey);
    const manifestBlock = await downloadBlock(manifestCid);
    if (!manifestBlock) throw new Error('Manifest not found');
    const manifest = vault.openManifest(envelope, manifestBlock, contentKey);
    const cids: string[] = manifest.shards;
    let fetched = 0;
    const shards = await Promise.all(
      cids.map(async (cid) => {
        const b = await downloadBlock(cid);
        fetched += 1;
        callbacks.onProgress?.((fetched / cids.length) * 90);
        return b;
      }),
    );
    const data: Uint8Array = vault.recover(manifest.json, contentKey, shards);
    callbacks.onProgress?.(100);
    callbacks.onSuccess?.(new Blob([data as unknown as BlobPart], { type: manifest.mime }), manifest.name);
  } catch (err) {
    callbacks.onError?.(err instanceof Error ? err.message : 'Download failed');
  }
};
