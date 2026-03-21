import logging
import sys

RESET = "\033[0m"
RED = "\033[91m"
YELLOW = "\033[93m"
GREEN = "\033[92m"
BLUE = "\033[94m"
GRAY = "\033[90m"

class ColoredFormatter(logging.Formatter):
    def format(self, record):
        levelname = record.levelname
        if levelname == "DEBUG":
            color = GRAY
        elif levelname == "INFO":
            color = BLUE
        elif levelname == "WARNING":
            color = YELLOW
        elif levelname == "ERROR":
            color = RED
        else:
            color = ""
        record.levelname = f"{color}{levelname}{RESET}"
        return super().format(record)

def setup_logger(name: str = "deepseek_web_api", level: int = logging.WARNING):
    logger = logging.getLogger(name)
    logger.setLevel(level)
    if not logger.handlers:
        handler = logging.StreamHandler(sys.stdout)
        handler.setLevel(level)
        formatter = ColoredFormatter(
            "%(levelname)s | %(name)s | %(message)s",
            style="%"
        )
        handler.setFormatter(formatter)
        logger.addHandler(handler)
    return logger

# Default logger instance (configured on import via setup_logger call in __init__.py)
logger = logging.getLogger("deepseek_web_api")
