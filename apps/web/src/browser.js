// What the web app needs from the browser: this device's key, a non-extractable WebCrypto P-256
// key kept in IndexedDB with the saved server, and same-origin requests.

const STORE = 'flicks';

let opened;

// The database, opened once. A failed open is tried again on the next use.
function db() {
  opened ??= new Promise((resolve, reject) => {
    const open = indexedDB.open('flicks', 1);
    open.onupgradeneeded = () => open.result.createObjectStore(STORE);
    open.onsuccess = () => resolve(open.result);
    open.onerror = () => reject(open.error);
  }).catch((error) => {
    opened = undefined;
    throw error;
  });
  return opened;
}

async function access(mode, use) {
  const store = (await db()).transaction(STORE, mode).objectStore(STORE);
  return new Promise((resolve, reject) => {
    const request = use(store);
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
}

export function load(name) {
  return access('readonly', (store) => store.get(name));
}

export async function save(name, value) {
  await access('readwrite', (store) => store.put(value, name));
}

let pair;

// The key pair, made on first use. The private key can sign but never be read.
function key() {
  pair ??= (async () => {
    let saved = await load('key');
    if (!saved) {
      saved = await crypto.subtle.generateKey({ name: 'ECDSA', namedCurve: 'P-256' }, false, ['sign']);
      await save('key', saved);
    }
    return saved;
  })();
  return pair;
}

// The public key, SEC1 uncompressed (65 bytes).
export async function publicKey() {
  return new Uint8Array(await crypto.subtle.exportKey('raw', (await key()).publicKey));
}

// An ECDSA P-256 SHA-256 signature, r ‖ s (64 bytes), as flicks_client::keys::sign makes.
export async function sign(message) {
  const signature = await crypto.subtle.sign({ name: 'ECDSA', hash: 'SHA-256' }, (await key()).privateKey, message);
  return new Uint8Array(signature);
}

// A same-origin request: [status, body bytes].
export async function request(method, path, authorization, body) {
  const headers = {};
  if (authorization) headers.Authorization = authorization;
  if (body.length) headers['Content-Type'] = 'application/json';
  const response = await fetch(path, { method, headers, body: body.length ? body : undefined, cache: 'no-store' });
  return [response.status, new Uint8Array(await response.arrayBuffer())];
}

let free = 6;
const waiting = [];

// Waits for one of six slots (the browser queues requests past its own connection limit, and a
// signature made before that wait would expire); resolves to the function that gives the slot back.
export function acquire() {
  return new Promise((resolve) => {
    const take = () => {
      free--;
      resolve(() => {
        free++;
        waiting.shift()?.();
      });
    };
    if (free > 0) take();
    else waiting.push(take);
  });
}
