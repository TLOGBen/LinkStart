#!/usr/bin/env python3
from __future__ import annotations

import importlib.util
import json
import re
import struct
import tempfile
import unittest
from argparse import Namespace
from pathlib import Path


MODULE_PATH = Path(__file__).with_name("release.py")
WORKFLOW_PATH = MODULE_PATH.parents[2] / ".github" / "workflows" / "release.yml"
SPEC = importlib.util.spec_from_file_location("linkstart_release", MODULE_PATH)
assert SPEC and SPEC.loader
release = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(release)


COMMIT = "0123456789abcdef0123456789abcdef01234567"
WORKFLOW_RUN = "https://github.com/TLOGBen/LinkStart/actions/runs/123/attempts/1"


def assert_common_dev_consumer_contract(manifest: dict) -> None:
    """Import-equivalent exact-key contract from common-dev runtime.py."""
    assert set(manifest) == {
        "schemaVersion",
        "runtimeVersion",
        "protocolMajor",
        "releaseTag",
        "artifacts",
    }
    assert manifest["schemaVersion"] == 1
    assert manifest["protocolMajor"] == "v1"
    assert manifest["releaseTag"] == f"v{manifest['runtimeVersion']}"
    assert isinstance(manifest["artifacts"], list)
    expected = {
        "target",
        "path",
        "size",
        "sha256",
        "sourceRepository",
        "sourceTag",
        "sourceCommit",
        "workflowRun",
    }
    for item in manifest["artifacts"]:
        assert set(item) == expected
        target = item["target"]
        assert item["path"] == f"bin/{target}/{release.TARGETS[target]['binary']}"
        assert item["sourceRepository"] == release.SOURCE_REPOSITORY
        assert item["sourceTag"] == manifest["releaseTag"]
        assert isinstance(item["workflowRun"], str) and item["workflowRun"]


def elf_x64(*, dynamic: bool = False) -> bytes:
    data = bytearray(128)
    data[:6] = b"\x7fELF\x02\x01"
    struct.pack_into("<H", data, 18, 62)
    struct.pack_into("<Q", data, 32, 64)
    struct.pack_into("<H", data, 54, 56)
    struct.pack_into("<H", data, 56, 1)
    struct.pack_into("<I", data, 64, 3 if dynamic else 1)
    return bytes(data)


def pe_x64() -> bytes:
    data = bytearray(256)
    data[:2] = b"MZ"
    struct.pack_into("<I", data, 0x3C, 128)
    data[128:132] = b"PE\0\0"
    struct.pack_into("<H", data, 132, 0x8664)
    return bytes(data)


def macho_universal() -> bytes:
    data = bytearray(64)
    data[:4] = b"\xca\xfe\xba\xbe"
    struct.pack_into(">I", data, 4, 2)
    struct.pack_into(">I", data, 8, 0x01000007)
    struct.pack_into(">I", data, 28, 0x0100000C)
    return bytes(data)


class ReleaseAssemblyTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.version_json = self.root / "version.json"
        self.version_json.write_text(
            json.dumps(
                {
                    "version": "0.1.1",
                    "protocolMajor": "v1",
                    "channel": "LinkStart v1 Preview：Stable core",
                }
            ),
            encoding="utf-8",
        )

    def tearDown(self) -> None:
        self.temp.cleanup()

    def create_bundles(self) -> Path:
        incoming = self.root / "incoming"
        fixtures = {
            "linux-x64-musl": ("linkstart", elf_x64(), "Linux", "X64"),
            "windows-x64": ("linkstart.exe", pe_x64(), "Windows", "X64"),
            "macos-universal": ("linkstart", macho_universal(), "macOS", "ARM64"),
        }
        for target, (name, content, runner_os, runner_arch) in fixtures.items():
            binary = self.root / f"fixture-{target}-{name}"
            binary.write_bytes(content)
            release.bundle(
                Namespace(
                    binary=str(binary),
                    target=target,
                    output=str(incoming / target),
                    version="0.1.1",
                    protocol_major="v1",
                    version_json=str(self.version_json),
                    source_commit=COMMIT,
                    source_tag="v0.1.1",
                    source_repository=release.SOURCE_REPOSITORY,
                    workflow_run=WORKFLOW_RUN,
                    runner_os=runner_os,
                    runner_arch=runner_arch,
                    rustc_version="rustc 1.88.0 (fixture)",
                    rust_target=release.TARGETS[target]["rust_targets"],
                )
            )
        return incoming

    def assemble_args(self, incoming: Path, output: Path) -> Namespace:
        return Namespace(
            input=str(incoming),
            output=str(output),
            version="0.1.1",
            protocol_major="v1",
            source_commit=COMMIT,
            source_tag="v0.1.1",
            source_repository=release.SOURCE_REPOSITORY,
            workflow_run=WORKFLOW_RUN,
        )

    def test_exact_layout_manifest_and_deterministic_archive(self) -> None:
        incoming = self.create_bundles()
        output = self.root / "assembled"
        release.assemble(self.assemble_args(incoming, output))
        manifest = json.loads((output / "assets" / "checksums.json").read_text(encoding="utf-8"))
        assert_common_dev_consumer_contract(manifest)
        self.assertEqual(
            [record["target"] for record in manifest["artifacts"]], sorted(release.TARGETS)
        )
        first = self.root / "one.zip"
        second = self.root / "two.zip"
        release.archive(Namespace(root=str(output), output=str(first)))
        release.archive(Namespace(root=str(output), output=str(second)))
        self.assertEqual(release.digest(first), release.digest(second))

    def test_common_dev_consumer_rejects_extra_keys(self) -> None:
        incoming = self.create_bundles()
        output = self.root / "assembled"
        release.assemble(self.assemble_args(incoming, output))
        manifest_path = output / "assets" / "checksums.json"
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
        manifest["extra"] = True
        with self.assertRaises(AssertionError):
            assert_common_dev_consumer_contract(manifest)
        manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
        with self.assertRaises(release.ReleaseError):
            release.validate_manifest_file(manifest_path, output)

    def test_common_dev_consumer_rejects_extra_artifact_keys(self) -> None:
        incoming = self.create_bundles()
        output = self.root / "assembled"
        release.assemble(self.assemble_args(incoming, output))
        manifest = json.loads((output / "assets" / "checksums.json").read_text(encoding="utf-8"))
        manifest["artifacts"][0]["extra"] = True
        with self.assertRaises(AssertionError):
            assert_common_dev_consumer_contract(manifest)
        manifest_path = output / "assets" / "checksums.json"
        manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
        with self.assertRaises(release.ReleaseError):
            release.validate_manifest_file(manifest_path, output)

    def test_tag_version_mismatch_is_rejected(self) -> None:
        with self.assertRaises(release.ReleaseError):
            release.validate_version("0.1.1", "v1", "v0.2.0")

    def test_publication_is_push_exact_tag_only(self) -> None:
        workflow = WORKFLOW_PATH.read_text(encoding="utf-8")
        match = re.search(r"(?m)^  release:\n    if: (.+)$", workflow)
        self.assertIsNotNone(match)
        self.assertEqual(
            match.group(1),
            "github.event_name == 'push' && github.ref_type == 'tag' && "
            "startsWith(github.ref, 'refs/tags/v')",
        )
        self.assertNotIn("gh release create", workflow[: match.start()])

    def test_dynamic_linux_binary_is_rejected(self) -> None:
        path = self.root / "dynamic"
        path.write_bytes(elf_x64(dynamic=True))
        with self.assertRaises(release.ReleaseError):
            release.validate_binary(path, "linux-x64-musl")

    def test_thin_macos_binary_is_rejected(self) -> None:
        path = self.root / "thin-macos"
        path.write_bytes(b"\xcf\xfa\xed\xfe" + bytes(60))
        with self.assertRaises(release.ReleaseError):
            release.validate_binary(path, "macos-universal")

    def test_wrong_version_json_is_rejected(self) -> None:
        self.version_json.write_text(
            json.dumps({"version": "0.2.0", "protocolMajor": "v1"}), encoding="utf-8"
        )
        with self.assertRaises(release.ReleaseError):
            release.validate_version_json(self.version_json, "0.1.1", "v1")

    def test_missing_target_is_rejected(self) -> None:
        incoming = self.create_bundles()
        target = incoming / "windows-x64"
        for child in target.iterdir():
            child.unlink()
        target.rmdir()
        with self.assertRaises(release.ReleaseError):
            release.assemble(self.assemble_args(incoming, self.root / "assembled"))

    def test_checksum_tamper_is_rejected(self) -> None:
        incoming = self.create_bundles()
        path = incoming / "windows-x64" / "linkstart.exe"
        path.write_bytes(path.read_bytes() + b"tamper")
        with self.assertRaises(release.ReleaseError):
            release.assemble(self.assemble_args(incoming, self.root / "assembled"))


if __name__ == "__main__":
    unittest.main()
