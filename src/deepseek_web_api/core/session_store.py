"""Session state store for mapping chat_session_id to parent_message_id."""

from threading import Lock


class SessionStore:
    _instance = None
    _lock = Lock()

    def __init__(self):
        self._sessions: dict[str, int | None] = {}  # chat_session_id -> parent_message_id

    @classmethod
    def get_instance(cls):
        if cls._instance is None:
            with cls._lock:
                if cls._instance is None:
                    cls._instance = cls()
        return cls._instance

    def create_session(self, chat_session_id: str) -> None:
        """Create new session with null parent_message_id."""
        with self._lock:
            self._sessions[chat_session_id] = None

    def get_parent_message_id(self, chat_session_id: str) -> int | None:
        """Get parent_message_id for session."""
        with self._lock:
            return self._sessions.get(chat_session_id)

    def update_parent_message_id(self, chat_session_id: str, message_id: int) -> None:
        """Update parent_message_id after receiving response."""
        with self._lock:
            self._sessions[chat_session_id] = message_id

    def delete_session(self, chat_session_id: str) -> bool:
        """Delete session, return True if existed."""
        with self._lock:
            if chat_session_id in self._sessions:
                del self._sessions[chat_session_id]
                return True
            return False

    def has_session(self, chat_session_id: str) -> bool:
        """Check if session exists."""
        with self._lock:
            return chat_session_id in self._sessions

    def get_all_sessions(self) -> list[str]:
        """Get all session IDs."""
        with self._lock:
            return list(self._sessions.keys())
