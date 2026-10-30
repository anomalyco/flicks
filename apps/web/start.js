import init from './app.js';
try {
    if (!navigator.gpu) throw new Error('Flicks needs a browser with WebGPU (Safari 26, Chrome or Edge), over HTTPS.');
    await init();
    document.querySelector('#status').remove();
} catch (error) {
    document.querySelector('#status').textContent = String(error);
    console.error(error);
}
