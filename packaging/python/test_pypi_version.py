"""Tests of pypi_version.py: `python -m pytest packaging/python`."""

from pathlib import Path

import pytest

from pypi_version import next_post, pep440, write_version


@pytest.mark.parametrize(
    ("tag", "want"),
    [
        ("v1.2.0", "1.2.0"),
        ("1.2.0", "1.2.0"),
        ("v1.2.0-rc1", "1.2.0rc1"),
        ("v1.2.0-beta.2", "1.2.0b2"),
        ("v2.0.0-alpha1", "2.0.0a1"),
        # Already PEP 440: unchanged.
        ("1.2.0.post1", "1.2.0.post1"),
        ("1.2.0rc1", "1.2.0rc1"),
    ],
)
def test_tags_become_pep440_versions(tag: str, want: str) -> None:
    assert pep440(tag) == want


@pytest.mark.parametrize("tag", ["main", "v1.2", "v1.2.0-dev.20261104.b46df06", "refs/tags/v1.2.0"])
def test_other_refs_are_rejected(tag: str) -> None:
    with pytest.raises(ValueError):
        pep440(tag)


def test_a_new_version_is_published_as_it_is() -> None:
    assert next_post("1.2.0", set()) == "1.2.0"
    assert next_post("1.2.0", {"1.1.0", "1.1.0.post1"}) == "1.2.0"


def test_a_released_version_gets_the_next_post_release() -> None:
    assert next_post("1.2.0", {"1.2.0"}) == "1.2.0.post1"
    assert next_post("1.2.0", {"1.2.0", "1.2.0.post1", "1.2.0.post3", "1.2.1.post7"}) == "1.2.0.post4"
    assert next_post("1.2.0rc1", {"1.2.0rc1"}) == "1.2.0rc1.post1"


def test_the_version_replaces_the_dynamic_one(tmp_path: Path) -> None:
    p = tmp_path / "pyproject.toml"
    p.write_text('[project]\nname = "x"\ndynamic = ["version"]\n')
    write_version(p, "1.2.0.post1")
    assert p.read_text() == '[project]\nname = "x"\nversion = "1.2.0.post1"\n'
    with pytest.raises(ValueError):
        write_version(p, "1.2.0.post2")


def test_the_real_pyproject_has_a_dynamic_version() -> None:
    pyproject = Path(__file__).resolve().parents[2] / "crates" / "lac-python" / "pyproject.toml"
    assert 'dynamic = ["version"]' in pyproject.read_text()
