"""Bounded follow-up context never persists and never splits a completed turn."""
import unittest
from unittest.mock import patch

from tempo_conversation import Conversation, MAX_CONTEXT_BYTES, MAX_TURNS
import tempo_client


class ConversationTests(unittest.TestCase):
    def test_followup_keeps_completed_turns_and_current_prompt(self):
        chat = Conversation()
        chat.commit("My name is Alex", "Hello Alex")
        self.assertEqual(chat.messages("What is my name?"), [
            {"role": "user", "content": "My name is Alex"},
            {"role": "assistant", "content": "Hello Alex"},
            {"role": "user", "content": "What is my name?"}])

    def test_preparing_failed_turn_does_not_commit_or_mutate_previous_context(self):
        chat = Conversation()
        chat.commit("first", "answer")
        chat.messages("failed request")
        self.assertNotIn("failed request", str(chat.messages("retry manually")))
        snapshot = chat.messages("retry manually")
        snapshot[0]["content"] = "tampered"
        self.assertEqual(chat.messages("next")[0]["content"], "first")

    def test_independent_windows_and_clear(self):
        one, two = Conversation(), Conversation()
        one.commit("secret", "answer")
        self.assertEqual(two.messages("hi"), [{"role": "user", "content": "hi"}])
        one.clear()
        self.assertEqual(one.messages("next"), [{"role": "user", "content": "next"}])

    def test_oldest_pairs_drop_under_turn_and_byte_limits(self):
        chat = Conversation()
        for index in range(MAX_TURNS + 3):
            chat.commit(str(index), "answer")
        messages = chat.messages("latest")
        self.assertEqual(len(messages), MAX_TURNS * 2 + 1)
        self.assertEqual(messages[0]["content"], "3")
        chat.commit("large", "x" * (MAX_CONTEXT_BYTES - 10))
        self.assertEqual(chat.messages("y" * 100), [{"role": "user", "content": "y" * 100}])

    def test_unicode_budget_counts_bytes_and_does_not_truncate_prompt(self):
        chat = Conversation()
        chat.commit("before", "answer")
        prompt = "🚀" * (MAX_CONTEXT_BYTES // 4)
        self.assertEqual(chat.messages(prompt), [{"role": "user", "content": prompt}])
        with self.assertRaises(ValueError):
            chat.messages(prompt + "🚀")
        self.assertEqual(chat.messages("after")[0]["content"], "before")

    def test_invalid_turn_is_atomic_and_large_answer_does_not_leave_old_context(self):
        chat = Conversation()
        chat.commit("before", "answer")
        with self.assertRaises(ValueError):
            chat.commit("invalid", "")
        self.assertEqual(chat.messages("next")[0]["content"], "before")
        chat.commit("newest", "x" * (MAX_CONTEXT_BYTES + 1))
        self.assertEqual(chat.messages("next"), [{"role": "user", "content": "next"}])

    def test_client_sends_all_turns_with_private_default_and_unchanged_controls(self):
        messages = Conversation().messages("synthetic")
        with patch.object(tempo_client, "request_json", return_value=({}, {})) as request:
            tempo_client.chat_messages("127.0.0.1", 8765, "token", messages, timeout=3)
        args, kwargs = request.call_args
        self.assertEqual(args[5]["messages"], messages)
        self.assertEqual(args[5]["metadata"], {"privacy_class": "private"})
        self.assertFalse(args[5]["stream"])
        self.assertEqual(kwargs["timeout"], 3)

    def test_invalid_client_messages_rejected_before_transport(self):
        cases = ([], "text", [{}], [{"role": "tool", "content": "x"}],
                 [{"role": [], "content": "x"}],
                 [{"role": "user", "content": 5}], [{"role": "user", "content": "x" * 32001}],
                 [{"role": "user", "content": "x"}] * 101,
                 [{"role": "user", "content": "\ud800"}])
        with patch.object(tempo_client, "request_json", side_effect=AssertionError("transport reached")):
            for messages in cases:
                with self.subTest(messages=str(messages)[:50]), self.assertRaises(ValueError):
                    tempo_client.chat_messages("127.0.0.1", 8765, "token", messages)


if __name__ == "__main__":
    unittest.main()
