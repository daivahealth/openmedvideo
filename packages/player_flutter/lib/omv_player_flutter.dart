/// Flutter wrapper for the OpenMedVideo clinical imaging player
/// (design §5.3, player distribution tier 3 — WebView variant).
///
/// One codebase, no parallel renderers: this widget embeds the server's
/// tier-1 player page (`/player/{token}/{studyId}`) in a WebView and bridges
/// the player's `omv-*` events to Dart callbacks over a JavaScript channel.
/// The clinical UI — slice scrub bar, frame stepping, window-preset tabs —
/// is the same one every other tier uses, so a fix ships to all apps at
/// once. A fully native `video_player`-based implementation remains a
/// separate, demand-gated effort per the design.
///
/// ```dart
/// OmvPlayer(
///   server: 'https://omv.example.org',
///   studyId: '1.2.840...',
///   token: playbackToken, // from GET /v1/studies/{uid}
///   onFrame: (f) => debugPrint('slice ${f.frame}/${f.frames}'),
/// )
/// ```
library omv_player_flutter;

import 'dart:convert';

import 'package:flutter/widgets.dart';
import 'package:webview_flutter/webview_flutter.dart';

/// A frame-change notification from the player.
@immutable
class OmvFrameEvent {
  const OmvFrameEvent({required this.frame, required this.frames});

  /// 1-based current frame (slice) number.
  final int frame;

  /// Total frames in the active rendition.
  final int frames;

  @override
  String toString() => 'OmvFrameEvent($frame/$frames)';
}

/// Builds the player page URL for a study. Exposed for testing and for apps
/// that want the raw URL (e.g. to open externally).
Uri omvPlayerUrl({
  required String server,
  required String studyId,
  required String token,
}) {
  final origin = server.replaceAll(RegExp(r'/+$'), '');
  return Uri.parse('$origin/player/$token/$studyId');
}

/// Parses one message from the page's OmvChannel bridge. Returns null for
/// malformed payloads (never throws on wire data). Exposed for testing.
({String type, Map<String, dynamic> detail})? parseOmvMessage(String raw) {
  try {
    final decoded = jsonDecode(raw);
    if (decoded is! Map<String, dynamic>) return null;
    final type = decoded['type'];
    if (type is! String) return null;
    final detail = decoded['detail'];
    return (
      type: type,
      detail: detail is Map<String, dynamic> ? detail : <String, dynamic>{},
    );
  } on FormatException {
    return null;
  }
}

/// Imperative control over an [OmvPlayer], obtained via [OmvPlayer.controller].
class OmvPlayerController {
  WebViewController? _web;

  /// Step ±n frames (pauses playback).
  Future<void> step(int delta) async =>
      _web?.runJavaScript("document.querySelector('omv-player')?.step($delta)");

  /// Jump to a 1-based frame number (pauses playback).
  Future<void> gotoFrame(int frame) async => _web?.runJavaScript(
      "document.querySelector('omv-player')?.gotoFrame($frame)");
}

class OmvPlayer extends StatefulWidget {
  const OmvPlayer({
    super.key,
    required this.server,
    required this.studyId,
    required this.token,
    this.controller,
    this.onReady,
    this.onError,
    this.onFrame,
  });

  /// OMV API origin, e.g. `https://omv.example.org`.
  final String server;

  /// DICOM StudyInstanceUID.
  final String studyId;

  /// Playback token from `GET /v1/studies/{uid}`.
  final String token;

  /// Optional imperative controller for frame stepping.
  final OmvPlayerController? controller;

  final void Function(String studyUid)? onReady;
  final void Function(String message)? onError;
  final void Function(OmvFrameEvent event)? onFrame;

  @override
  State<OmvPlayer> createState() => _OmvPlayerState();
}

class _OmvPlayerState extends State<OmvPlayer> {
  late final WebViewController _web;

  @override
  void initState() {
    super.initState();
    _web = WebViewController()
      ..setJavaScriptMode(JavaScriptMode.unrestricted)
      ..addJavaScriptChannel('OmvChannel',
          onMessageReceived: (m) => _dispatch(m.message))
      ..loadRequest(omvPlayerUrl(
        server: widget.server,
        studyId: widget.studyId,
        token: widget.token,
      ));
    widget.controller?._web = _web;
  }

  @override
  void didUpdateWidget(OmvPlayer old) {
    super.didUpdateWidget(old);
    if (old.server != widget.server ||
        old.studyId != widget.studyId ||
        old.token != widget.token) {
      _web.loadRequest(omvPlayerUrl(
        server: widget.server,
        studyId: widget.studyId,
        token: widget.token,
      ));
    }
  }

  void _dispatch(String raw) {
    final msg = parseOmvMessage(raw);
    if (msg == null) return;
    switch (msg.type) {
      case 'omv-ready':
        widget.onReady?.call(msg.detail['studyUid'] as String? ?? '');
      case 'omv-error':
        widget.onError?.call(msg.detail['message'] as String? ?? 'unknown');
      case 'omv-frame':
        final frame = msg.detail['frame'];
        final frames = msg.detail['frames'];
        if (frame is num && frames is num) {
          widget.onFrame?.call(
              OmvFrameEvent(frame: frame.toInt(), frames: frames.toInt()));
        }
    }
  }

  @override
  Widget build(BuildContext context) => WebViewWidget(controller: _web);
}
