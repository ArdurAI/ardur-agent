"""Exercise the shipping guard's refusal, including future workspace additions."""
import importlib.util
import unittest
from pathlib import Path

spec = importlib.util.spec_from_file_location('shipping', Path(__file__).with_name('check-shipping-set.py'))
shipping = importlib.util.module_from_spec(spec)
spec.loader.exec_module(shipping)


class ShippingSetTests(unittest.TestCase):
    def test_allowed_client_and_transport(self):
        self.assertEqual(shipping.rejected(shipping.ALLOWED | {'reqwest', 'rustls', 'tokio'}, shipping.ALLOWED), [])

    def test_each_execution_family_and_future_workspace_crate(self):
        for name in shipping.FORBIDDEN | {'provider-new', 'channel-new'}:
            for crate in [name, 'ardur-' + name]:
                with self.subTest(crate=crate):
                    self.assertEqual(shipping.rejected({crate}, set()), [crate])
        self.assertEqual(shipping.rejected({'new-executor'}, {'new-executor'}), ['new-executor'])


if __name__ == '__main__':
    unittest.main()
