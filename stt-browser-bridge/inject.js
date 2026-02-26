/**
 * Injected into the bridge browser page.
 * - When useRealDevice: true (Linux + PulseAudio): capture from OS virtual device via getUserMedia.
 * - When useRealDevice: false (macOS or no PulseAudio): receive PCM from bridge, play into an
 *   AudioContext MediaStreamDestination, run SpeechRecognition on that track (no enumerateDevices override).
 */
(function () {
  const sessionId = window.__STT_SESSION_ID__;
  const pageWsUrl = window.__STT_PAGE_WS_URL__;
  if (!sessionId || !pageWsUrl) return;

  const SAMPLE_RATE = 16000;
  let audioContext = null;
  let destinationNode = null;
  let virtualStream = null;
  let nextStartTime = 0;
  let recognition = null;
  let pageWs = null;
  let config = null;
  let speechStarted = false;
  let startRecognitionWhenReady = false;

  function send(obj) {
    if (pageWs && pageWs.readyState === 1) {
      pageWs.send(JSON.stringify(obj));
    }
  }
  function debug(msg) {
    send({ type: 'debug', message: msg });
  }

  function getOrCreateVirtualStream() {
    if (virtualStream) return virtualStream;
    try {
      audioContext = new (window.AudioContext || window.webkitAudioContext)({ sampleRate: SAMPLE_RATE });
      destinationNode = audioContext.createMediaStreamDestination();
      virtualStream = destinationNode.stream;
      nextStartTime = audioContext.currentTime;
    } catch (e) {
      console.error('[stt-bridge inject] AudioContext failed', e);
    }
    return virtualStream;
  }

  let trackLiveSent = false;
  function queuePcm(arrayBuffer) {
    if (!audioContext || !destinationNode) return;
    const int16 = new Int16Array(arrayBuffer);
    const float32 = new Float32Array(int16.length);
    for (let i = 0; i < int16.length; i++) float32[i] = int16[i] / 32768;
    const buffer = audioContext.createBuffer(1, float32.length, SAMPLE_RATE);
    buffer.copyToChannel(float32, 0);
    const source = audioContext.createBufferSource();
    source.buffer = buffer;
    source.connect(destinationNode);
    var start = nextStartTime < audioContext.currentTime ? audioContext.currentTime : nextStartTime;
    const duration = float32.length / SAMPLE_RATE;
    nextStartTime = start + duration;
    source.start(start);
    source.stop(start + duration);

    if (!trackLiveSent && virtualStream) {
      var t = virtualStream.getAudioTracks()[0];
      if (t && t.readyState === 'live' && t.kind === 'audio') {
        trackLiveSent = true;
        send({ type: 'track_live' });
      }
    }
    if (startRecognitionWhenReady && recognition && virtualStream) {
      var track = virtualStream.getAudioTracks()[0];
      if (track && track.readyState === 'live' && track.kind === 'audio') {
        startRecognitionWhenReady = false;
        try {
          recognition.start(track);
          debug('recognition.start(track) called (PCM stream, after gesture)');
        } catch (e) {
          send({ type: 'error', text: 'recognition.start(track): ' + (e.message || String(e)) });
        }
      }
    }
  }

  function getCaptureStream(captureDeviceLabel) {
    if (!captureDeviceLabel) {
      return navigator.mediaDevices.getUserMedia({ audio: true });
    }
    return navigator.mediaDevices.enumerateDevices().then(function (devices) {
      var audioInputs = devices.filter(function (d) { return d.kind === 'audioinput'; });
      debug('enumerateDevices: ' + audioInputs.length + ' audioinput(s): ' +
        audioInputs.map(function (d) { return d.label || '(no label)'; }).join(', '));
      const want = captureDeviceLabel.toLowerCase();
      const audioInput = audioInputs.find(function (d) {
        const label = (d.label || '').toLowerCase();
        return label === want || label.indexOf(want) !== -1 || want.indexOf(label) !== -1;
      });
      if (!audioInput) {
        debug('capture device not found: ' + captureDeviceLabel + ', using default');
        return navigator.mediaDevices.getUserMedia({ audio: true });
      }
      debug('using capture device: ' + audioInput.label);
      return navigator.mediaDevices.getUserMedia({ audio: { deviceId: { exact: audioInput.deviceId } } });
    });
  }

  function startRecognitionWithStream(stream) {
    const SpeechRecognition = window.SpeechRecognition || window.webkitSpeechRecognition;
    if (!SpeechRecognition) {
      send({ type: 'error', text: 'SpeechRecognition not available' });
      return;
    }
    recognition = new SpeechRecognition();
    recognition.continuous = true;
    recognition.interimResults = true;
    recognition.lang = (config.language === 'ja' ? 'ja-JP' : config.language) || 'ja-JP';
    recognition.onresult = function (e) {
      const result = e.results[e.resultIndex];
      const text = result[0].transcript;
      const isFinal = result.isFinal;
      debug('onresult isFinal=' + isFinal + ' text=' + (text || '').slice(0, 80));
      if (!speechStarted) {
        speechStarted = true;
        send({ type: 'speech_start', timestamp_ms: Date.now() });
      }
      if (isFinal) {
        send({ type: 'final', text: text, language: recognition.lang || 'ja', confidence: 1.0, duration_ms: 0 });
      } else {
        send({ type: 'partial', text: text });
      }
    };
    recognition.onend = function () {
      if (speechStarted) {
        send({ type: 'speech_end', timestamp_ms: Date.now(), duration_ms: 0 });
        speechStarted = false;
      }
    };
    recognition.onerror = function (e) {
      debug('onerror error=' + (e.error || '') + ' message=' + (e.message || ''));
      if (e.error === 'no-speech') return;
      send({ type: 'partial', text: '' });
    };

    try {
      recognition.start();
      debug('recognition.start() called (uses OS default mic = BlackHole)');
      send({ type: 'recognition_started' });
    } catch (e) {
      send({ type: 'error', text: 'recognition.start() failed: ' + (e.message || String(e)) });
    }
  }

  function startRecognitionDirect() {
    const SpeechRecognition = window.SpeechRecognition || window.webkitSpeechRecognition;
    if (!SpeechRecognition) {
      send({ type: 'error', text: 'SpeechRecognition not available' });
      return;
    }
    recognition = new SpeechRecognition();
    recognition.continuous = true;
    recognition.interimResults = true;
    recognition.lang = (config.language === 'ja' ? 'ja-JP' : config.language) || 'ja-JP';
    recognition.onresult = function (e) {
      const result = e.results[e.resultIndex];
      const text = result[0].transcript;
      const isFinal = result.isFinal;
      debug('onresult isFinal=' + isFinal + ' text=' + (text || '').slice(0, 80));
      if (!speechStarted) {
        speechStarted = true;
        send({ type: 'speech_start', timestamp_ms: Date.now() });
      }
      if (isFinal) {
        send({ type: 'final', text: text, language: recognition.lang || 'ja', confidence: 1.0, duration_ms: 0 });
      } else {
        send({ type: 'partial', text: text });
      }
    };
    recognition.onaudiostart = function () {
      debug('onaudiostart — Chrome is capturing audio');
    };
    recognition.onsoundstart = function () {
      debug('onsoundstart — Chrome detected sound');
    };
    recognition.onspeechstart = function () {
      debug('onspeechstart — Chrome detected speech');
    };
    recognition.onend = function () {
      debug('recognition.onend');
      if (speechStarted) {
        send({ type: 'speech_end', timestamp_ms: Date.now(), duration_ms: 0 });
        speechStarted = false;
      }
    };
    recognition.onerror = function (e) {
      debug('onerror error=' + (e.error || '') + ' message=' + (e.message || ''));
      if (e.error === 'no-speech') return;
      send({ type: 'partial', text: '' });
    };
    try {
      recognition.start();
      debug('recognition.start() called (direct, no getUserMedia)');
      send({ type: 'recognition_started' });
    } catch (e) {
      send({ type: 'error', text: 'recognition.start() failed: ' + (e.message || String(e)) });
    }
  }

  function startRecognitionNoTrack(stream) {
    const SpeechRecognition = window.SpeechRecognition || window.webkitSpeechRecognition;
    if (!SpeechRecognition) {
      send({ type: 'error', text: 'SpeechRecognition not available' });
      return;
    }
    recognition = new SpeechRecognition();
    recognition.continuous = true;
    recognition.interimResults = true;
    recognition.lang = (config.language === 'ja' ? 'ja-JP' : config.language) || 'ja-JP';
    recognition.onresult = function (e) {
      const result = e.results[e.resultIndex];
      const text = result[0].transcript;
      const isFinal = result.isFinal;
      debug('onresult isFinal=' + isFinal + ' text=' + (text || '').slice(0, 80));
      if (!speechStarted) {
        speechStarted = true;
        send({ type: 'speech_start', timestamp_ms: Date.now() });
      }
      if (isFinal) {
        send({ type: 'final', text: text, language: recognition.lang || 'ja', confidence: 1.0, duration_ms: 0 });
      } else {
        send({ type: 'partial', text: text });
      }
    };
    recognition.onend = function () {
      if (speechStarted) {
        send({ type: 'speech_end', timestamp_ms: Date.now(), duration_ms: 0 });
        speechStarted = false;
      }
    };
    recognition.onerror = function (e) {
      debug('onerror error=' + (e.error || '') + ' message=' + (e.message || ''));
      if (e.error === 'no-speech') return;
      send({ type: 'partial', text: '' });
    };
    try {
      recognition.start();
      debug('recognition.start() called (no track — relies on default PulseAudio source)');
    } catch (e) {
      send({ type: 'error', text: 'recognition.start() failed: ' + (e.message || String(e)) });
    }
  }

  function setupPcmMode() {
    getOrCreateVirtualStream();
    if (!recognition) {
      const SpeechRecognition = window.SpeechRecognition || window.webkitSpeechRecognition;
      if (!SpeechRecognition) {
        send({ type: 'error', text: 'SpeechRecognition not available' });
        return;
      }
      recognition = new SpeechRecognition();
      recognition.continuous = true;
      recognition.interimResults = true;
      recognition.lang = (config.language === 'ja' ? 'ja-JP' : config.language) || 'ja-JP';
      recognition.onresult = function (e) {
        const result = e.results[e.resultIndex];
        const text = result[0].transcript;
        const isFinal = result.isFinal;
        debug('onresult isFinal=' + isFinal + ' text=' + (text || '').slice(0, 80));
        if (!speechStarted) {
          speechStarted = true;
          send({ type: 'speech_start', timestamp_ms: Date.now() });
        }
        if (isFinal) {
          send({ type: 'final', text: text, language: recognition.lang || 'ja', confidence: 1.0, duration_ms: 0 });
        } else {
          send({ type: 'partial', text: text });
        }
      };
      recognition.onend = function () {
        if (speechStarted) {
          send({ type: 'speech_end', timestamp_ms: Date.now(), duration_ms: 0 });
          speechStarted = false;
        }
      };
      recognition.onerror = function (e) {
        debug('onerror error=' + (e.error || '') + ' message=' + (e.message || ''));
        if (e.error === 'no-speech') return;
        send({ type: 'partial', text: '' });
      };
    }
    if (audioContext && audioContext.state === 'suspended') audioContext.resume();
    startRecognitionWhenReady = true;
    var btn = document.getElementById('stt-start');
    if (btn) {
      btn.addEventListener('click', function onStart() {
        btn.removeEventListener('click', onStart);
        startRecognitionWhenReady = false;
        if (audioContext && audioContext.state === 'suspended') {
          audioContext.resume().then(function () {
            doStartWithTrack();
          }).catch(function () { doStartWithTrack(); });
        } else {
          doStartWithTrack();
        }
        function doStartWithTrack() {
          var track = virtualStream && virtualStream.getAudioTracks()[0];
          if (!track || track.readyState !== 'live' || track.kind !== 'audio' || !recognition) {
            debug('startRecognitionWhenReady set (track not live yet)');
            return;
          }
          function startWithTrack() {
            try {
              recognition.start(track);
              debug('recognition.start(track) called (PCM stream, user gesture)');
            } catch (e) {
              send({ type: 'error', text: 'recognition.start(track): ' + (e.message || String(e)) });
            }
          }
          navigator.mediaDevices.getUserMedia({ audio: true }).then(function (stream) {
            stream.getTracks().forEach(function (t) { t.stop(); });
            startWithTrack();
          }).catch(function () {
            startWithTrack();
          });
        }
      });
    }
    debug('PCM mode: waiting for Start STT click (user gesture)');
  }

  pageWs = new WebSocket(pageWsUrl + '?session=' + encodeURIComponent(sessionId));
  pageWs.binaryType = 'arraybuffer';
  pageWs.onopen = function () {
    if (config) pageWs.send(JSON.stringify({ type: 'config', ...config }));
  };

  pageWs.onmessage = function (ev) {
    if (typeof ev.data === 'string') {
      try {
        const msg = JSON.parse(ev.data);
        if (msg.type === 'config') {
          config = msg;
          debug('config received useRealDevice=' + config.useRealDevice);
          if (config.useRealDevice) {
            startRecognitionDirect();
          } else {
            setupPcmMode();
          }
        } else if (msg.type === 'end_of_stream') {
          if (speechStarted) {
            send({ type: 'speech_end', timestamp_ms: Date.now(), duration_ms: 0 });
            speechStarted = false;
          }
          if (recognition) {
            try { recognition.stop(); } catch (e) {}
          }
        }
      } catch (e) {}
    } else if (ev.data instanceof ArrayBuffer) {
      queuePcm(ev.data);
    }
  };
})();
