"""The committed dashboard is generated, and queries only metrics Rushls exports."""
import importlib.util
import json
from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location('build_dashboard', Path(__file__).with_name('build-dashboard.py'))
build = importlib.util.module_from_spec(spec)
spec.loader.exec_module(build)

HISTOGRAM_SUFFIX = re.compile(r'_(bucket|sum|count)$')


def exported_metrics():
    """Every `rushls_*` name the Rust source writes, as literals or prefix + suffix.

    The HTTP families are spelled `metric!("body_bytes_total")`, which expands
    to both `rushls_http_` and `rushls_stream_http_`, so suffixes passed to
    `metric!` are joined with every prefix the source passes to `concat!`.
    """
    names, prefixes, suffixes = set(), set(), set()
    for path in [*ROOT.glob('src/**/*.rs'), *ROOT.glob('crates/**/*.rs')]:
        text = path.read_text(encoding='utf-8')
        names.update(re.findall(r'"(rushls_[a-z0-9_]+[a-z0-9])"', text))
        prefixes.update(re.findall(r'concat!\("(rushls_[a-z0-9_]+_)"', text))
        suffixes.update(re.findall(r'metric!\("([a-z0-9_]+)"\)', text))
    names.update(prefix + suffix for prefix in prefixes for suffix in suffixes)
    return names


def queries(panels):
    for panel in panels:
        for target in panel.get('targets', []):
            yield panel['title'], target['expr']
        yield from queries(panel.get('panels', []))


class DashboardTests(unittest.TestCase):
    def test_committed_json_matches_the_generator(self):
        committed = (ROOT / 'examples/monitoring/dashboard.json').read_text(encoding='utf-8')
        self.assertEqual(committed, build.render(),
                         'regenerate with: python3 tools/build-dashboard.py > examples/monitoring/dashboard.json')

    def test_every_queried_metric_is_exported(self):
        exported = exported_metrics()
        self.assertIn('rushls_build_info', exported, 'the source scan found no metrics')
        for title, expr in queries(build.dashboard['panels']):
            for name in re.findall(r'\b(rushls_[a-z0-9_]+)', expr):
                with self.subTest(panel=title, metric=name):
                    self.assertIn(HISTOGRAM_SUFFIX.sub('', name), exported)

    def test_every_query_is_scoped_to_the_selected_nodes(self):
        # Without it, a multi-origin deployment would mix in nodes the user filtered out.
        for title, expr in queries(build.dashboard['panels']):
            for selector in re.findall(r'rushls_[a-z0-9_]+\{([^}]*)\}', expr):
                with self.subTest(panel=title, selector=selector):
                    self.assertIn('$node_label=~"$node"', selector)

    def test_panels_do_not_overlap(self):
        cells = {}
        for panel in build.dashboard['panels']:
            grid = panel['gridPos']
            self.assertLessEqual(grid['x'] + grid['w'], 24, panel['title'])
            for x in range(grid['x'], grid['x'] + grid['w']):
                for y in range(grid['y'], grid['y'] + grid['h']):
                    self.assertNotIn((x, y), cells, f"{panel['title']} overlaps {cells.get((x, y))}")
                    cells[(x, y)] = panel['title']

    def test_the_datasource_is_chosen_at_import(self):
        text = json.dumps(build.dashboard)
        self.assertNotIn('"uid": "prometheus"', text)
        self.assertNotIn('vm-production', text)


if __name__ == '__main__':
    unittest.main()
