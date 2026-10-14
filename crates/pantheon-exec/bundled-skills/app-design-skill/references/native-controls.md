# Native controls

Use the platform's native controls. A rebuilt control that behaves differently
reads as fake.

- iOS: UIKit controls (UISwitch, UISlider, UISegmentedControl)
- Android: Material components
- Web: native HTML elements (input, select, button)

Never rebuild a native control unless you can match its behavior exactly.
