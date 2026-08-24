import 'package:flutter_test/flutter_test.dart';
import 'package:omv_player_flutter/omv_player_flutter.dart';

void main() {
  group('omvPlayerUrl', () {
    test('builds the tier-1 player URL', () {
      final u = omvPlayerUrl(
        server: 'https://omv.example.org/',
        studyId: '1.2.3',
        token: 'tok.sig',
      );
      expect(u.toString(), 'https://omv.example.org/player/tok.sig/1.2.3');
    });
  });

  group('parseOmvMessage', () {
    test('parses bridge events', () {
      final m = parseOmvMessage(
          '{"type":"omv-frame","detail":{"frame":21,"frames":40}}');
      expect(m, isNotNull);
      expect(m!.type, 'omv-frame');
      expect(m.detail['frame'], 21);
      expect(m.detail['frames'], 40);
    });

    test('never throws on wire garbage', () {
      expect(parseOmvMessage('not json'), isNull);
      expect(parseOmvMessage('42'), isNull);
      expect(parseOmvMessage('{"detail":{}}'), isNull);
      expect(parseOmvMessage('{"type":"omv-ready"}')!.detail, isEmpty);
    });
  });
}
