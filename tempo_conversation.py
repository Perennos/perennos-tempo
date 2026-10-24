"""Per-window, volatile context. Complete oldest turn pairs are dropped first."""
from __future__ import annotations

from collections import deque

MAX_TURNS = 12
MAX_CONTEXT_BYTES = 12_000


def _size(text: str) -> int:
    if not isinstance(text, str) or not text.strip():
        raise ValueError("Message must contain text.")
    try:
        return len(text.encode("utf-8"))
    except UnicodeError:
        raise ValueError("Message must be valid Unicode text.") from None


class Conversation:
    def __init__(self):
        self._turns: deque[tuple[str, str]] = deque(maxlen=MAX_TURNS)

    def messages(self, prompt: str) -> list[dict[str, str]]:
        size = _size(prompt)
        if size > MAX_CONTEXT_BYTES:
            raise ValueError("Message is too long. Keep it within 12,000 UTF-8 bytes or send a shorter excerpt.")
        kept = []
        for user, answer in reversed(self._turns):
            pair_size = _size(user) + _size(answer)
            if size + pair_size > MAX_CONTEXT_BYTES:
                break
            size += pair_size
            kept.append((user, answer))
        messages = []
        for user, answer in reversed(kept):
            messages.extend(({"role": "user", "content": user}, {"role": "assistant", "content": answer}))
        return messages + [{"role": "user", "content": prompt}]

    def commit(self, prompt: str, answer: str):
        size = _size(prompt) + _size(answer)
        if _size(prompt) > MAX_CONTEXT_BYTES:
            raise ValueError("Message exceeds the conversation limit.")
        # An oversized answer breaks the contiguous recent context. Do not skip it
        # and accidentally resurrect an older topic in the next request.
        if size > MAX_CONTEXT_BYTES:
            self.clear()
            return
        self._turns.append((prompt, answer))
        while sum(_size(user) + _size(text) for user, text in self._turns) > MAX_CONTEXT_BYTES:
            self._turns.popleft()

    def clear(self):
        self._turns.clear()
