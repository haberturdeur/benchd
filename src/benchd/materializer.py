"""Making a claimed bench *appear* as a real device node, and vanish again.

This is the piece that makes normal tools work. ``esptool``, ``idf.py
monitor``, ``minicom`` and ``openocd`` all want a character device; an
``rfc2217://`` URL is a pyserial-only convenience that three of those four
will refuse. So a lease materialises actual device nodes and the agent uses
its normal toolchain against them.

Layout::

    <root>/<owner>/<lease-id>/<slot>/<resource>

The lease id is in the path on purpose. If ``/dev/lab/dut`` meant board A last
lease and board B this lease, a stale shell or backgrounded script could write
to the wrong board - which is the exact failure this system exists to prevent,
reintroduced through the back door. With the lease id in the path, a stale
reference fails with ``ENOENT`` instead of quietly corrupting someone else's
run.

Revocation is deliberately abrupt. When a lease ends the node goes away, and
in-flight I/O fails with ``EIO``/``ENOENT``. That is the correct behaviour -
it is what makes the lease real rather than advisory - but agents must be told
that these errors mean "your lease ended", not "the board is broken", or
they will start power-cycling hardware to fix an expired lease.
"""

from __future__ import annotations

import errno
import os
import shutil
import subprocess
from dataclasses import dataclass, field
from pathlib import Path
from typing import Mapping, Protocol

from .model import Bench, Resource, SerialResource

__all__ = [
    "MaterializedSlot",
    "MaterializedLease",
    "Materializer",
    "MaterializationError",
    "FakeMaterializer",
    "BindMountMaterializer",
    "env_var_name",
]


class MaterializationError(RuntimeError):
    """A lease could not be materialised."""


def env_var_name(slot: str, resource: str) -> str:
    """``dut`` + ``console`` -> ``LAB_DUT_CONSOLE``."""
    clean = lambda s: "".join(ch if ch.isalnum() else "_" for ch in s).upper()
    return f"LAB_{clean(slot)}_{clean(resource)}"


@dataclass(frozen=True)
class MaterializedSlot:
    """One slot's device nodes, as the holder sees them."""

    slot: str
    bench_id: str
    paths: dict[str, Path] = field(default_factory=dict)

    def env(self) -> dict[str, str]:
        return {
            env_var_name(self.slot, name): str(path)
            for name, path in self.paths.items()
        }


@dataclass(frozen=True)
class MaterializedLease:
    """Everything a holder needs to start working."""

    lease_id: str
    root: Path
    slots: dict[str, MaterializedSlot] = field(default_factory=dict)

    def env(self) -> dict[str, str]:
        out: dict[str, str] = {}
        for slot in self.slots.values():
            out.update(slot.env())
        return out


class Materializer(Protocol):
    """Backend that exposes and withdraws devices for a lease.

    Implementations must be idempotent: ``unmaterialize`` on an unknown or
    already-torn-down lease is a no-op, because the reaper will sometimes race
    a voluntary release and neither path may raise.
    """

    def materialize(
        self,
        lease_id: str,
        owner: str,
        slots: Mapping[str, Bench],
    ) -> MaterializedLease: ...

    def unmaterialize(self, lease_id: str, owner: str) -> None: ...


class FakeMaterializer:
    """In-memory/tmpdir backend for tests and for running without root.

    Creates ordinary empty files where device nodes would go, so path layout,
    env var generation and teardown can be tested without privileges or
    hardware.
    """

    def __init__(self, root: str | Path) -> None:
        self.root = Path(root)
        self.active: dict[str, MaterializedLease] = {}
        self.materialize_calls: list[str] = []
        self.unmaterialize_calls: list[str] = []

    def materialize(
        self,
        lease_id: str,
        owner: str,
        slots: Mapping[str, Bench],
    ) -> MaterializedLease:
        self.materialize_calls.append(lease_id)
        lease_root = self.root / owner / lease_id
        out: dict[str, MaterializedSlot] = {}
        for slot_name, bench in slots.items():
            slot_dir = lease_root / slot_name
            slot_dir.mkdir(parents=True, exist_ok=True)
            paths: dict[str, Path] = {}
            for res_name in bench.resource_names:
                target = slot_dir / res_name
                target.touch()
                paths[res_name] = target
            out[slot_name] = MaterializedSlot(
                slot=slot_name, bench_id=bench.id, paths=paths
            )
        lease = MaterializedLease(lease_id=lease_id, root=lease_root, slots=out)
        self.active[lease_id] = lease
        return lease

    def unmaterialize(self, lease_id: str, owner: str) -> None:
        self.unmaterialize_calls.append(lease_id)
        lease = self.active.pop(lease_id, None)
        if lease is None:
            return
        shutil.rmtree(lease.root, ignore_errors=True)


class BindMountMaterializer:
    """Bind-mounts real device nodes into a per-lease directory.

    Why bind mounts rather than symlinks: the holder's sandbox does not have
    ``/dev/ttyUSB0``, so a symlink pointing at it would dangle. A bind mount
    puts the actual device inode at the destination path, which works inside a
    sandbox that has ``<root>/<owner>`` bind-mounted in and no other access to
    ``/dev``.

    Requires ``CAP_SYS_ADMIN`` (in practice: run as root). The holder itself
    needs no privileges at all, which is the point - the daemon does the
    privileged work so agents never need it.

    Sandbox setup is out of scope here: the agent's sandbox is expected to have
    ``<root>/<owner>`` bind-mounted at (say) ``/dev/lab`` when it starts.
    Because it is a *directory* bind mount, entries created and removed by this
    class appear and disappear inside the running sandbox with no restart.
    """

    def __init__(
        self,
        root: str | Path = "/run/benchd/agents",
        mount_bin: str = "mount",
        umount_bin: str = "umount",
    ) -> None:
        self.root = Path(root)
        self.mount_bin = mount_bin
        self.umount_bin = umount_bin

    # -- helpers ---------------------------------------------------------

    def _lease_root(self, owner: str, lease_id: str) -> Path:
        return self.root / owner / lease_id

    def _resolve(self, bench: Bench, resource: Resource) -> Path:
        if not isinstance(resource, SerialResource):
            raise MaterializationError(
                f"bench {bench.id}: resource {resource.name!r} of kind "
                f"{resource.kind!r} is not supported by the bind-mount backend "
                "(remote resources need the USB/IP backend)"
            )
        resolved = resource.resolve()
        if resolved is None:
            raise MaterializationError(
                f"bench {bench.id}: {resource.by_id} is not present. "
                "The board may be unplugged; mark the bench disabled or "
                "re-seat the cable."
            )
        return resolved

    # -- Materializer protocol -------------------------------------------

    def materialize(
        self,
        lease_id: str,
        owner: str,
        slots: Mapping[str, Bench],
    ) -> MaterializedLease:
        lease_root = self._lease_root(owner, lease_id)

        # Resolve everything before mounting anything: a half-materialised
        # lease is worse than a failed one.
        plan: list[tuple[str, str, Path, Path]] = []
        for slot_name, bench in slots.items():
            for res_name in bench.resource_names:
                source = self._resolve(bench, bench.resources[res_name])
                dest = lease_root / slot_name / res_name
                plan.append((slot_name, res_name, source, dest))

        out: dict[str, dict[str, Path]] = {}
        try:
            for slot_name, res_name, source, dest in plan:
                dest.parent.mkdir(parents=True, exist_ok=True)
                # The bind-mount target must exist and be a file-like node.
                if not dest.exists():
                    dest.touch(mode=0o600)
                self._run([self.mount_bin, "--bind", str(source), str(dest)])
                out.setdefault(slot_name, {})[res_name] = dest
        except Exception:
            # Roll back so we never leave a partially mounted lease behind.
            self.unmaterialize(lease_id, owner)
            raise

        return MaterializedLease(
            lease_id=lease_id,
            root=lease_root,
            slots={
                slot: MaterializedSlot(
                    slot=slot, bench_id=slots[slot].id, paths=paths
                )
                for slot, paths in out.items()
            },
        )

    def unmaterialize(self, lease_id: str, owner: str) -> None:
        lease_root = self._lease_root(owner, lease_id)
        if not lease_root.exists():
            return

        # Unmount depth-first, then remove the tree. Failures are logged by the
        # caller but never raised: the reaper must always be able to finish.
        for path in sorted(lease_root.rglob("*"), reverse=True):
            if path.is_dir():
                continue
            self._run(
                [self.umount_bin, "--lazy", str(path)],
                check=False,
            )
        shutil.rmtree(lease_root, ignore_errors=True)
        # Tidy the owner directory if this was their last lease, but leave the
        # directory itself: it is bind-mounted into a running sandbox.
        try:
            owner_root = self.root / owner
            if owner_root.is_dir() and not any(owner_root.iterdir()):
                pass  # keep it; the sandbox holds a mount on it
        except OSError as exc:  # pragma: no cover - defensive
            if exc.errno != errno.ENOENT:
                raise

    def _run(self, cmd: list[str], check: bool = True) -> None:
        try:
            result = subprocess.run(
                cmd,
                capture_output=True,
                text=True,
                check=False,
            )
        except FileNotFoundError as exc:
            raise MaterializationError(f"{cmd[0]} not found") from exc
        if check and result.returncode != 0:
            raise MaterializationError(
                f"{' '.join(cmd)} failed ({result.returncode}): "
                f"{result.stderr.strip() or result.stdout.strip()}"
            )

    @staticmethod
    def available() -> bool:
        """True if this process can plausibly bind-mount (i.e. is root)."""
        return os.geteuid() == 0
