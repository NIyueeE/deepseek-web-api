"""OpenAI compatible API routes."""

from .models import router as models_router
from .chat_completions import router as chat_completions_router

__all__ = ["models_router", "chat_completions_router"]
