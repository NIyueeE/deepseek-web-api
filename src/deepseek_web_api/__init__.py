"""DeepSeek Web API package."""

import logging

from .api.routes import app
from .api.openai import models_router, chat_completions_router
from .core import init_single_account
from .core.logger import setup_logger

# Setup centralized logger (default WARNING = silent)
setup_logger(level=logging.WARNING)

# Include OpenAI compatible routers
app.include_router(models_router)
app.include_router(chat_completions_router)

# Initialize on import
init_single_account()

__all__ = ["app"]
