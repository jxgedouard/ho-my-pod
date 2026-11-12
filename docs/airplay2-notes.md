# AirPlay 2 sender notes

What Ho My Pod sends to a HomePod, step by step, and what was learned along the way. The sequence
follows [OwnTone](https://github.com/owntone/owntone-server)'s realtime AirPlay 2 path. Everything here
was checked against a HomePod (1st generation) and a HomePod mini.

## Discovery

Receivers advertise `_airplay._tcp`. The TXT record has `model` (`AudioAccessory1,1` is a HomePod,
`AudioAccessory5,1` a HomePod mini, `AudioAccessory6,1` a HomePod 2nd generation), `deviceid`, and
`features` as two 32-bit hex words (`0xLOW,0xHIGH`). Ho My Pod keeps receivers with feature bit 9 (audio)
and bit 48 (CoreUtils pairing, i.e. AirPlay 2).

## Session setup

All requests carry `DACP-ID`, `Active-Remote` and `Client-Instance` headers identifying the sender.

1. **`GET /info`**: returns `statusFlags`, which say whether a PIN or password is required.
2. **Transient pair-setup** (`POST /pair-setup`, header `X-Apple-HKP: 4`), TLV8 bodies:
   - M1: method 0, state 1, flags `0x10` (transient).
   - M2: salt and server public key `B`.
   - M3: client public key `A` and proof `M1`.
   - M4: server proof, checked against `H(A | M1 | K)`.

   SRP-6a with the RFC 5054 3072-bit group, `g = 5`, SHA-512, user `Pair-Setup`, password `3939`. Apple
   devices use the csrp conventions: `k = H(PAD(N) | PAD(g))`, `u = H(PAD(A) | PAD(B))`,
   `x = H(salt | H("Pair-Setup:3939"))` with leading zero bytes stripped from the salt, `K = H(S)`, and
   `M1 = H(H(N) xor H(g) | H(user) | salt | A | B | K)`.
3. **Encrypted control channel.** After pairing, the RTSP connection is encrypted:
   - keys come from HKDF-SHA512 over `K`, salt `Control-Salt`, infos `Control-Write-Encryption-Key` and
     `Control-Read-Encryption-Key`;
   - each frame is `u16 LE length | ChaCha20-Poly1305(ciphertext) | tag`, with the length used as AAD and a
     64-bit little-endian counter as nonce (bytes 4..12);
   - blocks are at most 1024 bytes.
4. **`SETUP` (session)**: a binary plist with `deviceID`, `sessionUUID`, `timingPort`,
   `timingProtocol: NTP` and `name`. The reply gives the `eventPort`.
5. **Events channel.** The sender opens a TCP connection to `eventPort`. It is encrypted the same way
   with salt `Events-Salt`, and the read and write keys are swapped. The receiver sends RTSP requests
   over it, and each one gets a `200 OK` reply. This is also where Siri commands arrive (see below).
6. **`RECORD`**.
7. **`SETUP` (stream)**: `type 96` (realtime), `audioFormat 0x40000` (ALAC 44100/16/2), `ct 2`,
   `spf 352`, `latencyMin 11025`, `latencyMax 88200`, `shk` = the first 32 bytes of `K`, and our control
   port. The reply gives the data and control ports.
8. **`SET_PARAMETER`** `volume: <dB>`. AirPlay volume runs from -30 dB (quietest) to 0 dB (maximum), and
   -144 means mute. `GET_PARAMETER volume` reads back the speaker's own volume, which is how the app
   follows changes made with Siri or touch.
9. **`POST /feedback`** every 2 seconds keeps the session alive. **`TEARDOWN`** (with an empty plist dict)
   ends it.

## Audio

- RTP payload type `0x60`, 352 frames per packet, each one an ALAC "escape" frame. Escape frames carry
  uncompressed 16-bit PCM behind an ALAC header, so no encoder is needed.
- Each payload is sealed with ChaCha20-Poly1305 using `shk`. The AAD is the RTP header bytes 4..12. The
  packet ends with the 16-byte tag and the last 8 bytes of the nonce (the sequence number, little endian).
- The first packet sent to a receiver has the marker bit set.
- Sync packets go on the control port once per second (`0x90 0xd4` the first time, then `0x80 0xd4`).
  Each one carries the current RTP time minus the latency, the NTP time, and the next RTP time.
- Receivers ask for lost packets with `0x80 0xd5` on the control port. The sender answers from a
  1024-packet history.
- Timing: the receiver sends NTP-style requests (`0x80 0xd2`) to our timing port and expects
  `0x80 0xd3` replies with our clock. **If these never arrive, for example because Windows Firewall drops
  inbound UDP on a "Public" network, the stream `SETUP` simply times out.** This was the first big
  blocker.

## Remote commands (Siri, touch, Control Center)

There are two routes, and HomePods use the second one:

- **DACP** (classic AirPlay): the sender advertises `_dacp._tcp` as `iTunes_Ctrl_<DACP-ID>`, and the
  receiver makes HTTP requests such as `GET /ctrl-int/1/playpause` with the `Active-Remote` token.
- **Events channel** (AirPlay 2): the HomePod sends `POST /command` with a binary plist:

  ```
  type: sendMediaRemoteCommand
  value: "paus"                       (four-letter code: play, paus, stop, nitm, pitm...)
  modernMediaRemoteCommand: "1"       (MRMediaRemoteCommand: 0 play, 1 pause, 2 toggle,
                                       3 stop, 4 next, 5 previous)
  params: { kMRMediaRemoteOptionSenderID: ... AssistantServices ..., ... }
  ```

Ho My Pod handles both and forwards the command to the current Windows media session.

## Windows capture

- WASAPI loopback in shared mode, converted by Windows to float32 / 44.1 kHz / stereo.
- On outputs with **software** volume, the loopback signal is attenuated by the master volume. Capturing
  in float lets the app undo that with no loss, through `IAudioEndpointVolume`.
- On outputs with **hardware** volume (`QueryHardwareSupport`), loopback is not attenuated.
- **A muted endpoint still delivers the full signal to loopback**, on both kinds of output. Ho My Pod
  mutes the PC while streaming and unmutes it afterwards. A marker file in `%APPDATA%\HoMyPod` makes the
  next launch unmute it if the app was killed meanwhile.
- A small jitter buffer (25 ms prefill, 70 ms high-water mark) absorbs the drift between the sound
  card clock and the sender's packet clock, by dropping a frame now and then.
