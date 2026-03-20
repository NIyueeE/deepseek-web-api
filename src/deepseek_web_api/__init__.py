"""DeepSeek Web API package."""

from .api.routes import app
from .core import init_single_account

# Initialize on import
init_single_account()

__all__ = ["app"]
