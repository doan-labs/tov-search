import collections
import pathlib
import tempfile
import unittest

import inject


class ModulesTest(unittest.TestCase):
    def test_cyclic_import_aliases_visit_each_canonical_file_once(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            entry = root / "engine.tov"
            a = root / "sub" / "a.tov"
            b = root / "sub" / "b.tov"
            a.parent.mkdir()
            entry.write_text("import { a } from './sub/a'\n" "import { again } from './sub/../sub/a'\n")
            a.write_text("import { b } from '../sub/../sub/b'\n")
            b.write_text("import { a } from '../sub/./a'\n")

            found = inject.modules(str(entry))

            self.assertEqual(
                collections.Counter(path for path, _ in found),
                collections.Counter(str(path.resolve()) for path in (entry, a, b)),
            )

    def test_named_multiline_and_namespace_imports_with_both_quotes(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            entry = root / "engine.tov"
            named = root / "named.tov"
            namespace = root / "namespace.tov"
            entry.write_text(
                "import {\n  first,\n  second,\n} from './named'\n"
                'import * as ns from "./namespace"\n'
                "import { readFile } from 'node:fs'\n"
            )
            named.write_text("")
            namespace.write_text("")

            found = inject.modules(str(entry))

            self.assertEqual(
                [path for path, _ in found],
                [str(path.resolve()) for path in (entry, named, namespace)],
            )

    def test_nonexistent_relative_import_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            entry = pathlib.Path(tmp) / "engine.tov"
            entry.write_text("import * as missing from './missing'\n")

            with self.assertRaisesRegex(SystemExit, "does not exist"):
                inject.modules(str(entry))


class ParseTovTest(unittest.TestCase):
    def test_duplicate_source_global_without_emitted_c_is_rejected(self):
        mods = [
            ("/tmp/first.tov", "let unused: int = 1\n"),
            ("/tmp/second.tov", "export const unused = 2\n"),
        ]

        with self.assertRaisesRegex(SystemExit, "global unused is defined"):
            inject.parse_tov(mods, "")

    def test_duplicate_hook_name_is_rejected(self):
        hook = "// hook: duplicate name\nexport function duplicate(x: int): int {\n  return x\n}\n"
        mods = [("/tmp/first.tov", hook), ("/tmp/second.tov", hook)]

        with self.assertRaisesRegex(SystemExit, "hook duplicate is defined"):
            inject.parse_tov(mods, "")

    def test_ambiguous_emitted_global_names_are_rejected(self):
        mods = [("/tmp/module.tov", "let value: int = 1\n")]
        c = "static tv_int g1_value;\nstatic tv_int g2_value;\n"

        with self.assertRaisesRegex(SystemExit, "matches 2 globals"):
            inject.parse_tov(mods, c)

    def test_cross_module_reads_are_allowed_but_writes_are_rejected(self):
        first = ("/tmp/first.tov", "let foreign: int = 1\n")
        second = (
            "/tmp/second.tov",
            "let own: int = 2\n"
            "// hook: reads foreign and updates own\n"
            "// reads: foreign\n"
            "// writes: own\n"
            "export function update(value: int): int {\n"
            "  return value\n"
            "}\n",
        )
        c = "static tv_int g0_foreign;\nstatic tv_int g1_own;\n"

        hooks, globals_ = inject.parse_tov([first, second], c)

        self.assertEqual([hook["name"] for hook in hooks], ["update"])
        self.assertEqual(globals_["foreign"]["c"], "g0_foreign")
        self.assertEqual(globals_["own"]["c"], "g1_own")

        cross_write = (
            second[0],
            second[1].replace("// reads: foreign\n", "").replace("// writes: own", "// writes: foreign"),
        )
        with self.assertRaisesRegex(SystemExit, "writes foreign"):
            inject.parse_tov([first, cross_write], c)

    def test_nonzero_module_signature_and_shadow_shim_use_module_c_names(self):
        mods = [
            ("/tmp/entry.tov", ""),
            (
                "/tmp/module.tov",
                "let values: int[] = []\n"
                "// hook: updates values\n"
                "// writes: values\n"
                "export function update(n: int): int {\n"
                "  return n\n"
                "}\n",
            ),
        ]
        c = "static tv_arr g2_values;\n" "static tv_int f2_update(tv_int p0) {\n" "  return p0;\n" "}\n"

        hooks, globals_ = inject.parse_tov(mods, c)
        signature = inject.check_c_signature(hooks[0], c)
        sig = signature.group(0)[:-3]
        shadow = inject.shim(hooks[0], globals_, sig, shadow=True, wide=False)

        self.assertEqual(hooks[0]["c"], "f2_update")
        self.assertIn("f2_update__ref(p0)", shadow)
        self.assertIn("&g2_values", shadow)
        self.assertIn("tv_arr_data(g2_values)", shadow)
        self.assertNotIn("f0_update", shadow)
        self.assertNotIn("g0_values", shadow)


if __name__ == "__main__":
    unittest.main()
