"""Stateless session pool for v1 chat completions.

This module provides a session pool that enables stateless behavior while using
DeepSeek's edit_message API with fixed message_id=1.

Architecture:
- Each session in the pool maintains message_id=1
- The pool is "stateless" from the client's perspective (no session_id needed)
- Internally, we track which sessions are initialized (have message_id=1 created)
- Pool size is hard-capped; callers block until a session is available or timeout
"""

import asyncio
import time
from dataclasses import dataclass, field
from typing import Optional

from ....core.logger import logger


@dataclass
class StatelessSession:
    """A session that tracks whether message_id=1 has been initialized.

    Attributes:
        chat_session_id: The DeepSeek session ID
        is_initialized: Whether message_id=1 exists in this session
        last_access_time: Timestamp of last access (for cleanup)
        lock: Per-session lock to prevent concurrent use
    """

    chat_session_id: str
    is_initialized: bool = False
    last_access_time: float = field(default_factory=time.time)
    lock: asyncio.Lock = field(default_factory=lambda: asyncio.Lock())


class SessionPoolError(Exception):
    """Base exception for session pool errors."""

    pass


class SessionPoolFullError(SessionPoolError):
    """Raised when the pool is at capacity and no session becomes available within the timeout."""

    pass


class StatelessSessionPool:
    """Pool of sessions for stateless chat completions.

    This pool manages a set of DeepSeek sessions, each tracking whether
    message_id=1 has been initialized. Sessions are reused across requests
    to minimize session creation overhead.

    The pool provides:
    - Hard cap on concurrent sessions (pool_size) to avoid triggering DeepSeek rate limits
    - Callers block in acquire() until a session is available or acquire_timeout expires
    - Per-session locking to prevent concurrent use
    - Lazy initialization (first request uses completion, subsequent use edit_message)
    - Automatic cleanup of idle sessions
    - Error recovery (mark session as needing re-init on error)
    """

    def __init__(
        self,
        max_idle_seconds: float = 300,
        pool_size: int = 10,
        acquire_timeout: float = 30.0,
    ):
        """Initialize the session pool.

        Args:
            max_idle_seconds: Sessions idle longer than this are eligible for cleanup
            pool_size: Hard cap on concurrent sessions; callers wait when this is reached
            acquire_timeout: Seconds to wait for an available session before raising
        """
        self._sessions: dict[str, StatelessSession] = {}
        self._condition = asyncio.Condition()
        self._pending_creates: int = 0  # Sessions being created (reserved slots)
        self._max_idle_seconds = max_idle_seconds
        self._pool_size = pool_size
        self._acquire_timeout = acquire_timeout
        self._cleanup_task: Optional[asyncio.Task] = None
        logger.info(
            f"[StatelessSessionPool] initialized with pool_size={pool_size}, "
            f"max_idle={max_idle_seconds}s, acquire_timeout={acquire_timeout}s"
        )

    async def acquire(self) -> StatelessSession:
        """Acquire an available session from the pool.

        Returns the first available (unlocked) session. If the pool is below
        capacity, creates a new session. If the pool is at capacity and all
        sessions are busy, blocks until one is released or the timeout expires.

        Returns:
            StatelessSession: An available session (already locked)

        Raises:
            SessionPoolFullError: If no session becomes available within acquire_timeout
            SessionPoolError: If session creation fails
        """
        deadline = asyncio.get_event_loop().time() + self._acquire_timeout

        while True:
            # Phase 1: Check pool state under condition lock
            async with self._condition:
                # Try to find an available unlocked session
                for session in self._sessions.values():
                    if not session.lock.locked():
                        await session.lock.acquire()
                        session.last_access_time = time.time()
                        logger.debug(f"[pool] acquired existing session {session.chat_session_id[:8]}...")
                        return session

                # Can we grow the pool? (count pending creates as reserved slots)
                effective_size = len(self._sessions) + self._pending_creates
                if effective_size < self._pool_size:
                    # Reserve a slot; will create outside the lock below
                    self._pending_creates += 1
                    logger.info(
                        f"[pool] growing pool: {len(self._sessions)} sessions + "
                        f"{self._pending_creates} pending = {effective_size + 1}/{self._pool_size}"
                    )
                    # Fall through to Phase 2 after exiting this block
                else:
                    # Pool at capacity, all sessions busy — wait for a release notification
                    remaining = deadline - asyncio.get_event_loop().time()
                    if remaining <= 0:
                        raise SessionPoolFullError(
                            f"Pool exhausted: all {self._pool_size} sessions busy, "
                            f"timed out after {self._acquire_timeout:.1f}s"
                        )
                    logger.info(
                        f"[pool] all {self._pool_size} sessions busy, "
                        f"waiting up to {remaining:.1f}s for one to free up..."
                    )
                    try:
                        await asyncio.wait_for(self._condition.wait(), timeout=remaining)
                    except asyncio.TimeoutError:
                        raise SessionPoolFullError(
                            f"Pool exhausted: all {self._pool_size} sessions busy, "
                            f"timed out after {self._acquire_timeout:.1f}s"
                        )
                    continue  # Woken by notify_all() — re-check at top of loop

            # Phase 2: Create new session outside lock (avoids blocking other acquires during HTTP call)
            try:
                new_session = await self._create_session()
            except Exception as e:
                async with self._condition:
                    self._pending_creates -= 1
                    self._condition.notify_all()  # Free the reserved slot for waiters
                raise SessionPoolError(f"Failed to create DeepSeek session: {e}") from e

            # Phase 3: Register in pool and lock it
            async with self._condition:
                self._pending_creates -= 1
                self._sessions[new_session.chat_session_id] = new_session
                await new_session.lock.acquire()
                logger.info(
                    f"[pool] created session {new_session.chat_session_id[:8]}..., "
                    f"pool size={len(self._sessions)}"
                )
                return new_session

    async def release(self, session: StatelessSession, error: bool = False) -> None:
        """Release a session back to the pool.

        Args:
            session: The session to release
            error: If True, mark the session as needing re-initialization
                   (e.g., due to message_id error from DeepSeek)
        """
        if error:
            session.is_initialized = False
            logger.warning(f"[pool] session {session.chat_session_id[:8]}... marked for re-init due to error")

        session.last_access_time = time.time()

        async with self._condition:
            session.lock.release()
            self._condition.notify_all()  # Wake any callers waiting in acquire()

        logger.debug(f"[pool] released session {session.chat_session_id[:8]}..., error={error}")

    async def cleanup_idle(self) -> int:
        """Remove sessions that have been idle too long.

        Returns:
            int: Number of sessions removed
        """
        now = time.time()
        removed = 0

        async with self._condition:
            to_remove = [
                sid
                for sid, session in self._sessions.items()
                if now - session.last_access_time > self._max_idle_seconds
                and not session.lock.locked()  # Don't remove busy sessions
            ]

            for sid in to_remove:
                del self._sessions[sid]

        if to_remove:
            from ...v0_service import delete_session as delete_remote_session

            for sid in to_remove:
                try:
                    resp = await delete_remote_session(sid)
                    if resp.status_code >= 400:
                        logger.warning(
                            f"[pool] remote cleanup for session {sid[:8]}... returned {resp.status_code}"
                        )
                except Exception as e:
                    logger.warning(f"[pool] failed to delete remote session {sid[:8]}...: {e}")
                removed += 1
                logger.info(f"[pool] removed idle session {sid[:8]}...")

        if removed > 0:
            logger.info(f"[pool] cleanup removed {removed} sessions, {len(self._sessions)} remaining")

        return removed

    async def _cleanup_loop(self):
        """Background loop that periodically cleans up idle sessions."""
        # Run first cleanup after half the idle timeout
        await asyncio.sleep(self._max_idle_seconds / 2)
        while True:
            try:
                removed = await self.cleanup_idle()
                if removed > 0:
                    logger.debug(f"[pool] cleanup loop removed {removed} idle sessions")
            except Exception as e:
                logger.warning(f"[pool] cleanup loop error: {e}")
            # Then run cleanup every half timeout
            await asyncio.sleep(self._max_idle_seconds / 2)

    def start_cleanup(self):
        """Start the background cleanup loop (fire and forget)."""
        if self._cleanup_task is None or self._cleanup_task.done():
            self._cleanup_task = asyncio.create_task(self._cleanup_loop())
            logger.info("[pool] started background cleanup loop")

    async def stop_cleanup(self):
        """Stop the background cleanup loop."""
        if self._cleanup_task:
            self._cleanup_task.cancel()
            try:
                await self._cleanup_task
            except asyncio.CancelledError:
                pass
            self._cleanup_task = None
            logger.info("[pool] stopped background cleanup loop")

    async def _create_session(self) -> StatelessSession:
        """Create a new DeepSeek session.

        Returns:
            StatelessSession: A new (uninitialized) session
        """
        # Import here to avoid circular imports
        from ...v0_service import create_session

        chat_session_id, resp = await create_session()
        if not chat_session_id:
            raise SessionPoolError("Failed to create session with DeepSeek")

        return StatelessSession(
            chat_session_id=chat_session_id,
            is_initialized=False,
            last_access_time=time.time(),
        )

    @property
    def size(self) -> int:
        """Return current number of sessions in the pool."""
        return len(self._sessions)

    @property
    def available_count(self) -> int:
        """Return number of available (unlocked) sessions."""
        return sum(1 for s in self._sessions.values() if not s.lock.locked())


# Global session pool instance
_pool: Optional[StatelessSessionPool] = None
_pool_lock = asyncio.Lock()


async def get_pool() -> StatelessSessionPool:
    """Get or create the global session pool.

    Returns:
        StatelessSessionPool: The global pool instance
    """
    global _pool
    if _pool is None:
        async with _pool_lock:
            if _pool is None:
                from ....core.config import get_pool_size, get_pool_acquire_timeout, get_max_idle_seconds
                _pool = StatelessSessionPool(
                    pool_size=get_pool_size(),
                    acquire_timeout=get_pool_acquire_timeout(),
                    max_idle_seconds=get_max_idle_seconds(),
                )
                _pool.start_cleanup()
    return _pool


async def cleanup_pool() -> int:
    """Cleanup idle sessions in the global pool.

    Returns:
        int: Number of sessions removed
    """
    pool = await get_pool()
    return await pool.cleanup_idle()
