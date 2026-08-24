# omv_player_flutter

Flutter wrapper for the OpenMedVideo clinical imaging player. Embeds the
server's player page in a WebView (one UI codebase across all platforms) and
bridges its events and controls to Dart.

```yaml
dependencies:
  omv_player_flutter:
    git:
      url: https://github.com/daivahealth/openmedvideo.git
      path: packages/player_flutter
```

```dart
final controller = OmvPlayerController();

OmvPlayer(
  server: 'https://omv.example.org',
  studyId: study.uid,
  token: playbackToken,          // from GET /v1/studies/{uid}
  controller: controller,        // controller.step(1) / gotoFrame(n)
  onReady: (uid) => ...,
  onError: (msg) => ...,         // e.g. expired token -> re-fetch the study
  onFrame: (f) => setState(() => slice = '${f.frame}/${f.frames}'),
)
```

Notes:

- Playback tokens are short-lived; on `onError`, re-fetch
  `GET /v1/studies/{uid}` for a fresh `player_url`/token and rebuild.
- The design's fully native `video_player`-based implementation is a
  separate, demand-gated effort; this wrapper is the supported path today.
- Status: statically verified (`flutter analyze` clean, unit tests for the
  URL builder and event bridge). Not yet exercised on a physical device —
  do a device smoke test before shipping in a clinical app.
