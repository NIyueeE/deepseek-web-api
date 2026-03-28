"""DeepSeek Web API package."""

from .api.routes import app
from .api.openai import models_router, chat_completions_router
from .core.logger import setup_logger

# Setup centralized logger (level configured in config.toml or default WARNING)
setup_logger()

# Include OpenAI compatible routers
app.include_router(models_router)
app.include_router(chat_completions_router)

# Note: Authentication is lazy - token is obtained on first API call
# See core/auth.py get_token()

__all__ = ["app"]
