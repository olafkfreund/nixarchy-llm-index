// expect: 3
import QtQuick
Item {
  // console.log("in a comment is fine")
  Component.onCompleted: console.log("x")
  function f() { console.warn(1); print ("y") }
}
