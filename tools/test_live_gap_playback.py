"""Regression checks for live rendition-switch evidence."""
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location(
    'live_gap_probe', Path(__file__).with_name('check-live-gap-playback.py'))
probe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(probe)


def request(kind='audio', **changes):
    return dict(dict(at=7.8, kind=kind, index=1, attempted=True, accepted=True,
                     previous=0, transitionStart=0), **changes)


def event(kind='audio', **changes):
    return dict(dict(event='hlsAudioTrackSwitched' if kind == 'audio' else 'hlsLevelSwitched',
                     id=1, level=1, selected=1), **changes)


class SwitchValidationTests(unittest.TestCase):
    def test_all_scheduled_switches_are_required(self):
        requests = probe.switch_schedule('all')
        requests[0].update(request())
        self.assertFalse(probe.validate_switches(requests, [event()]))
        self.assertTrue(requests[0]['observed'])
        self.assertEqual(requests[-1]['failure'], 'scheduled switch not attempted')

    def test_unavailable_or_unchanged_target_cannot_pass(self):
        for changes in ({'accepted': False}, {'previous': 1}, {'previous': -1},
                        {'previous': None}, {'transitionStart': None}):
            with self.subTest(changes=changes):
                self.assertFalse(probe.validate_switches([request(**changes)], [event()]))

    def test_initial_or_stale_event_cannot_satisfy_request(self):
        self.assertFalse(probe.validate_switches([request(transitionStart=1)], [event()]))
        self.assertTrue(probe.validate_switches([request(transitionStart=1)], [event(), event()]))

    def test_event_must_match_kind_target_and_selected_rendition(self):
        for wrong in (event('video'), event(id=0), event(selected=0)):
            with self.subTest(event=wrong):
                self.assertFalse(probe.validate_switches([request()], [wrong]))
        self.assertTrue(probe.validate_switches([request('video')], [event('video')]))

    def test_native_requires_only_requested_audio_track_enabled(self):
        for selected in ([], [0], [0, 1]):
            self.assertFalse(probe.validate_switches([request()], [
                dict(event='nativeAudioChanged', selected=selected)]))
        self.assertTrue(probe.validate_switches([request()], [
            dict(event='nativeAudioChanged', selected=[1])]))

    def test_both_directions_need_separate_events(self):
        requests = [request(), request(index=0, previous=1, transitionStart=1)]
        self.assertFalse(probe.validate_switches(requests, [event()]))
        self.assertTrue(probe.validate_switches(requests, [event(), event(id=0, selected=0)]))

    def test_later_request_cannot_supply_earlier_missing_evidence(self):
        requests = [request(), request(index=0, previous=1, transitionStart=0)]
        self.assertFalse(probe.validate_switches(requests, [event(), event(id=0, selected=0)]))
        self.assertFalse(requests[0]['observed'])

    def test_fixed_level_requires_observed_consistent_selection(self):
        self.assertTrue(probe.validate_fixed_level(None, []))
        self.assertFalse(probe.validate_fixed_level(0, []))
        self.assertTrue(probe.validate_fixed_level(0, [event('video', level=0)]))
        self.assertFalse(probe.validate_fixed_level(0, [event('video', level=0), event('video', level=1)]))

    def test_no_switch_case_is_explicit(self):
        self.assertEqual(probe.switch_schedule('none'), [])
        self.assertTrue(probe.validate_switches([], []))
        self.assertEqual([r['kind'] for r in probe.switch_schedule('audio')], ['audio', 'audio'])


if __name__ == '__main__':
    unittest.main()
