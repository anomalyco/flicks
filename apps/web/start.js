import init from './app.js';

// The intro's sound, decoded while the app loads. Played only where the browser already allows
// sound without a gesture (the context runs) and the tab is in front; otherwise the intro is silent.
const audio = new AudioContext();
const sound = fetch('./intro.wav').then((r) => r.arrayBuffer()).then((b) => audio.decodeAudioData(b)).catch(() => null);
let gain = null;
let stopped = false;
const close = () => {
    if (audio.state !== 'closed') audio.close().catch(() => {});
};
globalThis.flicksIntroSound = async (cue) => {
    if (cue === 'stop') {
        // A skip fades the sound out, then lets the device go.
        stopped = true;
        gain?.gain.setTargetAtTime(0, audio.currentTime, 0.01);
        setTimeout(close, gain ? 100 : 0);
        return;
    }
    if (audio.state !== 'running' || document.visibilityState !== 'visible') return close();
    const asked = performance.now();
    const buffer = await sound;
    // Skipped while it decoded: stay silent (the skip closes the context).
    if (stopped) return;
    // Decoding late starts it where the animation already is.
    const late = (performance.now() - asked) / 1000;
    if (!buffer || late > 0.3) return close();
    const source = audio.createBufferSource();
    source.buffer = buffer;
    gain = audio.createGain();
    source.connect(gain).connect(audio.destination);
    source.onended = close;
    source.start(0, late);
};
try {
    if (!navigator.gpu) throw new Error('Flicks needs a browser with WebGPU (Safari 26, Chrome or Edge), over HTTPS.');
    await init();
    document.querySelector('#status').remove();
} catch (error) {
    document.querySelector('#status').textContent = String(error);
    console.error(error);
}
