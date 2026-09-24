"""Local-only regression tests for the bundled skill-installer script.

Run manually; these tests are not wired into CI:
    python3 -B -m unittest discover -s codex-rs/skills/tests -p 'test_*.py'
"""

import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock
import urllib.error


INSTALLER = (
    Path(__file__).resolve().parents[1]
    / "src"
    / "assets"
    / "samples"
    / "skill-installer"
    / "scripts"
    / "install-skill-from-github.py"
)
SCRIPTS = INSTALLER.parent
sys.path.insert(0, str(SCRIPTS))
import github_utils  # noqa: E402


class SkillInstallerGitHubRequestTests(unittest.TestCase):
    def test_retries_anonymously_when_configured_token_is_rejected(self) -> None:
        requests = []

        def urlopen(request):
            requests.append(request)
            if len(requests) == 1:
                raise urllib.error.HTTPError(
                    request.full_url, 401, "Unauthorized", {}, None
                )
            return mock.MagicMock(
                __enter__=mock.Mock(
                    return_value=mock.Mock(read=mock.Mock(return_value=b"public"))
                ),
                __exit__=mock.Mock(return_value=False),
            )

        with mock.patch.dict(os.environ, {"GITHUB_TOKEN": "expired"}):
            with mock.patch.object(github_utils.urllib.request, "urlopen", urlopen):
                result = github_utils.github_request(
                    "https://api.github.com/repos/openai/skills/contents/skills/.curated",
                    "skill-installer-test",
                )

        self.assertEqual(result, b"public")
        self.assertEqual(requests[0].get_header("Authorization"), "token expired")
        self.assertIsNone(requests[1].get_header("Authorization"))


class SkillInstallerSymlinkTests(unittest.TestCase):
    def setUp(self) -> None:
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.repository = self.root / "repository"
        self.skill = self.repository / "skill"
        self.skill.mkdir(parents=True)
        (self.skill / "SKILL.md").write_text("Synthetic test skill\n", encoding="utf-8")
        self.destination = self.root / "installed"

    def create_symlink(self, link: Path, target: Path | str) -> None:
        try:
            link.symlink_to(target)
        except OSError as error:
            self.skipTest(f"symlinks are unavailable: {error}")

    def run_installer(self) -> subprocess.CompletedProcess[str]:
        subprocess.run(
            ["git", "init", "--initial-branch=main", str(self.repository)],
            check=True,
            capture_output=True,
            text=True,
        )
        subprocess.run(
            ["git", "-C", str(self.repository), "add", "."],
            check=True,
            capture_output=True,
            text=True,
        )
        subprocess.run(
            [
                "git",
                "-C",
                str(self.repository),
                "-c",
                "user.name=Skill Installer Test",
                "-c",
                "user.email=skill-installer@example.invalid",
                "commit",
                "-m",
                "synthetic skill fixture",
            ],
            check=True,
            capture_output=True,
            text=True,
        )

        environment = os.environ.copy()
        python_path = [str(INSTALLER.parent)]
        if environment.get("PYTHONPATH"):
            python_path.append(environment["PYTHONPATH"])
        environment.update(
            {
                "GIT_CONFIG_COUNT": "2",
                "GIT_CONFIG_KEY_0": f"url.{self.repository.as_uri()}.insteadOf",
                "GIT_CONFIG_VALUE_0": "https://github.com/synthetic/fixture.git",
                "GIT_CONFIG_KEY_1": "core.symlinks",
                "GIT_CONFIG_VALUE_1": "true",
                "GIT_TERMINAL_PROMPT": "0",
                "PYTHONPATH": os.pathsep.join(python_path),
            }
        )
        return subprocess.run(
            [
                sys.executable,
                "-B",
                str(INSTALLER),
                "--repo",
                "synthetic/fixture",
                "--path",
                "skill",
                "--method",
                "git",
                "--dest",
                str(self.destination),
            ],
            capture_output=True,
            text=True,
            check=False,
            env=environment,
        )

    def test_rejects_symlink_to_file_outside_skill(self) -> None:
        outside_file = self.root / "synthetic-secret.txt"
        outside_file.write_text("synthetic secret\n", encoding="utf-8")
        self.create_symlink(self.skill / "outside.txt", outside_file)

        result = self.run_installer()

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Unsupported symbolic link", result.stderr)
        self.assertFalse((self.destination / "skill").exists())

    def test_installs_symlink_to_regular_file_inside_skill(self) -> None:
        (self.skill / "actual.txt").write_text(
            "safe skill contents\n", encoding="utf-8"
        )
        self.create_symlink(self.skill / "alias.txt", "actual.txt")

        result = self.run_installer()

        self.assertEqual(result.returncode, 0, result.stderr)
        installed_alias = self.destination / "skill" / "alias.txt"
        self.assertFalse(installed_alias.is_symlink())
        self.assertEqual(
            installed_alias.read_text(encoding="utf-8"), "safe skill contents\n"
        )


if __name__ == "__main__":
    unittest.main()
