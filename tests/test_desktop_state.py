"""Exercise desktop state transitions without a display or model download."""
import queue
import threading
import unittest
from unittest.mock import Mock, patch

from tempo_conversation import Conversation, MAX_CONTEXT_BYTES
from tempo_desktop import TempoDesktop


class DesktopStateTests(unittest.TestCase):
    def desktop(self):
        desk = TempoDesktop.__new__(TempoDesktop)
        desk.conversation = Conversation()
        desk.pending_prompt = None
        desk.last_answer = ""
        desk.busy = False
        desk.busy_started = None
        desk.events = queue.Queue()
        desk.cancel_event = None
        desk.connection_lock = threading.Lock()
        desk.root = Mock()
        desk.status = Mock()
        desk.entry = Mock()
        desk.transcript = Mock()
        desk.send_button = Mock()
        desk.cancel_button = Mock()
        desk.new_chat_button = Mock()
        desk.copy_answer_button = Mock()
        desk.client = Mock()
        desk.token = "synthetic"
        desk.port = 8765
        desk._append = Mock()
        return desk

    def test_success_commits_context_but_error_and_cancel_do_not(self):
        for kind in ("success", "error", "cancelled"):
            with self.subTest(kind=kind):
                desk = self.desktop()
                desk.pending_prompt = "question"
                desk.events.put((kind, "answer" if kind != "cancelled" else False, {}))
                desk._poll()
                messages = desk.conversation.messages("next")
                self.assertEqual(len(messages), 3 if kind == "success" else 1)
                self.assertIsNone(desk.pending_prompt)

    def test_oversized_prompt_stays_in_editor_and_is_not_sent(self):
        desk = self.desktop()
        desk.entry.get.return_value = "x" * (MAX_CONTEXT_BYTES + 1)
        with patch("tempo_desktop.threading.Thread") as worker:
            desk.send()
        desk.entry.delete.assert_not_called()
        worker.assert_not_called()
        self.assertFalse(desk.busy)

    def test_clear_while_busy_preserves_context_and_answer(self):
        desk = self.desktop()
        desk.conversation.commit("previous", "answer")
        desk.last_answer = "answer"
        desk.busy = True
        desk.new_conversation()
        self.assertEqual(len(desk.conversation.messages("next")), 3)
        self.assertEqual(desk.last_answer, "answer")

    def test_clear_while_idle_drops_context_and_clipboard_is_explicit(self):
        desk = self.desktop()
        desk.conversation.commit("previous", "answer")
        desk.last_answer = "answer"
        desk.new_conversation()
        self.assertEqual(len(desk.conversation.messages("next")), 1)
        self.assertEqual(desk.last_answer, "")
        desk.entry.edit_reset.assert_called_once()
        desk.root.clipboard_append.assert_not_called()

    def test_copy_answer_is_explicit_and_never_copies_receipt_or_prompt(self):
        desk = self.desktop()
        desk.last_answer = "visible answer only"
        desk.copy_answer()
        desk.root.clipboard_append.assert_called_once_with("visible answer only")

    def test_background_status_does_not_replace_active_request_status(self):
        desk = self.desktop()
        desk.busy = True
        desk.cancel_event = threading.Event()
        desk.cancel_event.set()
        desk.events.put(("status", "Gateway reachable", {}))
        desk._poll()
        desk.status.set.assert_not_called()

    def test_invalid_unicode_answer_is_rejected_before_ui_commit(self):
        desk = self.desktop()
        desk.client.chat_messages.return_value = ({"choices": [{"message": {"content": "\ud800"}}]}, {})
        desk._send_worker([{"role": "user", "content": "question"}], threading.Event())
        event = desk.events.get_nowait()
        self.assertEqual(event[0], "error")
        desk.pending_prompt = "question"
        desk.events.put(event)
        desk._poll()
        desk.root.after.assert_called_once()
        self.assertIsNone(desk.pending_prompt)
        self.assertFalse(desk.busy)


if __name__ == "__main__":
    unittest.main()
