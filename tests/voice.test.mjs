// Rehearsal for the voice helpers: WAV bytes, resampling, levels, who a spoken line is for.
// Run: node --test tests/voice.test.mjs
import test from 'node:test';
import assert from 'node:assert/strict';
import { encodeWav, resample, downmix, levelOf, addressTo, wakeWord, speakable, RATE } from '../tempo-agent/gui-chat/voice.mjs';

const sine = (hz, rate, secs, amp = 0.5) => Float32Array.from({ length: Math.round(rate * secs) }, (_, i) => amp * Math.sin(2 * Math.PI * hz * i / rate));

test('a WAV has the header whisper.cpp wants: 16 kHz, mono, 16-bit PCM', () => {
  const w = encodeWav(sine(440, RATE, 0.5));
  const v = new DataView(w.buffer);
  const tag = (o) => String.fromCharCode(...w.slice(o, o + 4));
  assert.equal(tag(0), 'RIFF'); assert.equal(tag(8), 'WAVE'); assert.equal(tag(12), 'fmt '); assert.equal(tag(36), 'data');
  assert.equal(v.getUint16(20, true), 1);            // PCM
  assert.equal(v.getUint16(22, true), 1);            // mono
  assert.equal(v.getUint32(24, true), 16000);        // rate
  assert.equal(v.getUint32(28, true), 32000);        // bytes per second
  assert.equal(v.getUint16(34, true), 16);           // bits
  assert.equal(v.getUint32(40, true), 8000 * 2);     // data bytes: half a second
  assert.equal(v.getUint32(4, true), w.length - 8);  // RIFF size
  assert.equal(w.length, 44 + 16000);
});

test('samples are clipped, not wrapped, and silence is zero', () => {
  const w = encodeWav(Float32Array.from([0, 1, -1, 2, -2, 0.5]));
  const v = new DataView(w.buffer);
  const s = (i) => v.getInt16(44 + i * 2, true);
  assert.deepEqual([s(0), s(1), s(2), s(3), s(4)], [0, 32767, -32768, 32767, -32768]);
  assert.ok(Math.abs(s(5) - 16383) <= 1);
});

test('48 kHz down to 16 kHz keeps the length, the pitch and the loudness', () => {
  const x = sine(440, 48000, 1);
  const y = resample(x, 48000, RATE);
  assert.equal(y.length, 16000);
  const crossings = (a) => { let n = 0; for (let i = 1; i < a.length; i++) if (a[i - 1] < 0 && a[i] >= 0) n++; return n; };
  assert.ok(Math.abs(crossings(y) - 440) <= 2, 'still 440 Hz');
  assert.ok(Math.abs(levelOf(y) - levelOf(x)) < 0.05, 'same level');
});

test('44.1 kHz works too (not a whole ratio), and 8 kHz goes up', () => {
  const y = resample(sine(300, 44100, 1), 44100, RATE);
  assert.equal(y.length, 16000);
  const up = resample(sine(300, 8000, 1), 8000, RATE);
  assert.equal(up.length, 16000);
  assert.ok(Math.max(...up) > 0.45 && Math.max(...up) <= 0.5 + 1e-6);
  assert.equal(resample(y, RATE), y, 'already 16 kHz: untouched');
});

test('a tone above the new Nyquist is squashed, not folded back as a loud alias', () => {
  const y = resample(sine(15000, 48000, 0.5), 48000, RATE);
  assert.ok(levelOf(y) < 0.2, 'heavily reduced: ' + levelOf(y));
});

test('stereo mixes down to mono', () => {
  const m = downmix([Float32Array.from([1, 0]), Float32Array.from([0, 1])]);
  assert.deepEqual(Array.from(m), [0.5, 0.5]);
});

test('level: silence is 0, loud is capped at 1', () => {
  assert.equal(levelOf(new Float32Array(100)), 0);
  assert.equal(levelOf(new Float32Array(0)), 0);
  assert.equal(levelOf(new Float32Array(100).fill(1)), 1);
  assert.ok(levelOf(sine(200, 16000, 0.1, 0.05)) > 0.1);
});

test('in a Room, speech goes to @tempo unless it already names someone', () => {
  const o = { mention: '@tempo', names: ['tempo', 'sam'] };
  assert.equal(addressTo("what's on my list today", o), "@tempo what's on my list today");
  assert.equal(addressTo('@sam are you free', o), '@sam are you free');
  assert.equal(addressTo('at Sam are you free', o), '@sam are you free');
  assert.equal(addressTo('hey at tempo summarise this', o), '@tempo summarise this');
  assert.equal(addressTo('  ', o), '');
  assert.equal(addressTo('plain chat', { mention: null }), 'plain chat');
});

test('"Hey Tempo" is heard with or without a command after it', () => {
  assert.deepEqual(wakeWord('Hey Tempo.'), { hit: true, rest: '' });
  assert.deepEqual(wakeWord('Hey, Tempo, what is next today?'), { hit: true, rest: 'what is next today' });
  assert.equal(wakeWord('okay tempo play something').hit, true);
  assert.equal(wakeWord('the tempo of this song').hit, false);
  assert.equal(wakeWord('').hit, false);
});

test('replies are cleaned before they are read aloud', () => {
  assert.equal(speakable('Here: ```rm -rf x``` **done** see https://example.test/x and [docs](https://example.test).'), 'Here: done see and docs.');
  const long = ('Sentence number one is here. ').repeat(80);
  const s = speakable(long, 500);
  assert.ok(s.length <= 500 && s.endsWith('.'));
});
