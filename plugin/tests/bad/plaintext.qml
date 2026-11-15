// expect: 5
import QtQuick
import QtQuick as Q
Item {
  Text {
    textFormat: Text.PlainText
    text: "fine"
  }
  Text {
    text: "missing textFormat"
  }
  Text {
    text: untrusted
  } Text { textFormat: Text.PlainText; text: "a sibling does not count" }
  Text {
    text: privateName
    Text { textFormat: Text.PlainText }
  }
  Q.Text { text: qualified }
  Text { text: 'textFormat: Text.PlainText' }
}
