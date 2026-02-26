/**
 * Real virtual audio device: create an OS-level device and play PCM to it.
 * - Linux: PulseAudio null-sink (sink + monitor). PCM is played to the sink;
 *   the browser captures from the monitor via getUserMedia.
 * - macOS: User installs BlackHole etc. PCM is played to default output via sox
 *   (user sets default output to BlackHole); browser captures from BlackHole via getUserMedia.
 */

const { spawn, spawnSync } = require('child_process');

const SINK_NAME = 'stt_bridge';
const SAMPLE_RATE = 16000;
const CHANNELS = 1;
/** Label shown in the OS / enumerateDevices(); PulseAudio monitor shows "Monitor of <sink_name>". */
const CAPTURE_DEVICE_LABEL = 'Monitor of stt_bridge';
/** macOS: default label for BlackHole (Chrome lists this as microphone when BlackHole is installed). */
const MACOS_CAPTURE_DEVICE_LABEL = 'BlackHole 2ch';

let pulseModuleIndex = null;

/**
 * Create a PulseAudio null-sink (and its monitor) for STT.
 * On macOS, just returns captureDeviceLabel (no device creation).
 * @returns {{ captureDeviceLabel: string }} label to pass to the page for getUserMedia
 * @throws if pactl fails (Linux, PulseAudio not running or not available)
 */
function create() {
  if (process.platform !== 'linux') {
    return { captureDeviceLabel: process.env.STT_BRIDGE_CAPTURE_DEVICE_LABEL || MACOS_CAPTURE_DEVICE_LABEL };
  }
  if (pulseModuleIndex != null) return { captureDeviceLabel: CAPTURE_DEVICE_LABEL };

  const args = [
    'load-module', 'module-null-sink',
    'sink_name=' + SINK_NAME,
    'rate=' + SAMPLE_RATE,
    'channels=' + CHANNELS,
  ];
  const out = spawnSync('pactl', args, { encoding: 'utf8' });
  if (out.status !== 0) {
    const stderr = (out.stderr || '').trim();
    throw new Error('pactl load-module failed: ' + (stderr || out.error || 'unknown'));
  }
  pulseModuleIndex = parseInt(String(out.stdout).trim(), 10);
  if (!Number.isFinite(pulseModuleIndex)) {
    pulseModuleIndex = null;
    throw new Error('pactl did not return module index');
  }
  // Set the null-sink monitor as default source so Chromium sees it as an audioinput.
  const setDefault = spawnSync('pactl', ['set-default-source', SINK_NAME + '.monitor'], { encoding: 'utf8' });
  if (setDefault.status === 0) {
    console.error('[virtual-device] set default source to ' + SINK_NAME + '.monitor');
  }
  return { captureDeviceLabel: CAPTURE_DEVICE_LABEL };
}

function destroy() {
  if (process.platform !== 'linux' || pulseModuleIndex == null) return;
  try {
    spawnSync('pactl', ['unload-module', String(pulseModuleIndex)], { encoding: 'utf8' });
  } finally {
    pulseModuleIndex = null;
  }
}

/**
 * Check if playback is available (Linux: sink created; macOS: sox in PATH).
 */
function isPlaybackAvailable() {
  if (process.platform === 'linux') return pulseModuleIndex != null;
  if (process.platform === 'darwin') {
    const r = spawnSync('sox', ['-h'], { encoding: 'utf8' });
    return r.status === 0;
  }
  return false;
}

/**
 * Start a process that plays raw PCM (s16le 16kHz mono) to the virtual sink (Linux)
 * or default output device (macOS; user should set default to BlackHole).
 * @returns {{ write: (chunk: Buffer) => void, end: () => void }}
 */
function createPlayback() {
  if (process.platform === 'darwin') {
    const outDevice = process.env.STT_BRIDGE_PLAYBACK_DEVICE;
    const soxArgs = outDevice
      ? ['-t', 'raw', '-r', String(SAMPLE_RATE), '-e', 'signed', '-b', '16', '-c', String(CHANNELS), '-', '-t', 'coreaudio', outDevice]
      : ['-t', 'raw', '-r', String(SAMPLE_RATE), '-e', 'signed', '-b', '16', '-c', String(CHANNELS), '-', '-d'];
    const sox = spawn('sox', soxArgs, { stdio: ['pipe', 'ignore', 'pipe'] });
    sox.stderr.on('data', (d) => console.error('[bridge sox]', d.toString()));
    sox.on('error', (e) => console.error('[bridge sox error]', e));
    sox.stdin.on('error', (e) => {
      if (e.code !== 'EPIPE') console.error('[bridge sox stdin]', e.message || e);
    });
    sox.on('close', (code, signal) => {
      if (code !== 0 && code != null) console.error('[bridge sox] exited', code, signal || '');
    });
    return {
      write(chunk) {
        if (sox.stdin.writable) {
          try {
            sox.stdin.write(chunk, (err) => {
              if (err && err.code !== 'EPIPE') console.error('[bridge sox write]', err.message || err);
            });
          } catch (e) {
            if (e.code !== 'EPIPE') console.error('[bridge sox write]', e.message || e);
          }
        }
      },
      end() {
        if (sox.stdin.writable) {
          try {
            sox.stdin.end();
          } catch (e) {
            if (e.code !== 'EPIPE') console.error('[bridge sox end]', e.message || e);
          }
        }
      },
    };
  }
  if (process.platform !== 'linux') {
    return { write() {}, end() {} };
  }
  const paplay = spawn('paplay', [
    '-d', SINK_NAME,
    '--raw',
    '--format=s16le',
    '--rate=' + SAMPLE_RATE,
    '--channels=' + CHANNELS,
  ], { stdio: ['pipe', 'ignore', 'pipe'] });
  paplay.stderr.on('data', (d) => console.error('[bridge paplay]', d.toString()));
  paplay.on('error', (e) => console.error('[bridge paplay error]', e));
  return {
    write(chunk) {
      if (paplay.stdin.writable) paplay.stdin.write(chunk);
    },
    end() {
      if (paplay.stdin.writable) paplay.stdin.end();
    },
  };
}

module.exports = { create, destroy, createPlayback, isPlaybackAvailable, CAPTURE_DEVICE_LABEL };
