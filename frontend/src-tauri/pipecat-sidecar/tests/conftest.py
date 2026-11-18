import pytest
from loguru import logger


class _Captured:
    def __init__(self):
        self.lines: list[str] = []

    @property
    def text(self) -> str:
        return "\n".join(self.lines)


@pytest.fixture
def caplog_loguru():
    """Loguru does not go through `logging`, so pytest's caplog misses it."""
    cap = _Captured()
    sink = logger.add(lambda m: cap.lines.append(str(m)), level="DEBUG")
    yield cap
    logger.remove(sink)
