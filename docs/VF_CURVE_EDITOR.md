# Nvidia VF curve editor

The VF curve editor is available on the OC page for Nvidia GPUs that expose a
voltage-frequency curve. Enable "Enable VF Curve Edit" there to make the curve
editable.

The editor relies on undocumented driver functionality. Use it at your own risk.

## Frequency grid

The driver only accepts frequencies on a fixed grid, 15 MHz on the cards this
was tested on. A value between two grid lines is rounded by the driver, so the
curve that comes back after applying differs from the one that was drawn.

The editor detects the grid step from the frequencies reported by the driver
and snaps every edit to it. As a result, a line between two points is only
perfectly straight when its slope is a multiple of the step per point.
Otherwise neighbouring points alternate between two step sizes, and no point is
further than half a step from the ideal line.

## Selecting points

| Action | Result |
| --- | --- |
| Click a point | Select that point |
| Drag across an empty part of the chart | Select all points in that voltage range |
| Ctrl + click a point | Add the point to the selection or remove it |
| Ctrl + drag across the chart | Add the points in that range to the selection |
| Left / Right | Move the selection to the neighbouring point |
| Shift + Left / Right | Extend the selection |
| Ctrl + A | Select all visible points |
| Esc | Clear the selection |

Clicking a point no longer moves it. A point only moves once it is dragged.

## Moving points

| Action | Result |
| --- | --- |
| Drag a selected point | Move all selected points |
| Shift + drag a point | Move all visible points |
| Up / Down | Move the selection by one grid step |
| Ctrl + Up / Down, Page Up / Page Down | Move the selection by five grid steps |

Without a selection, Up and Down act on the point under the mouse pointer.
Only visible points are moved by the keyboard.

## Zooming and scrolling

| Action | Result |
| --- | --- |
| Mouse wheel | Zoom in or out around the pointer |
| + / - | Zoom in or out around the selected point |
| 0 | Show the whole curve |
| Shift + mouse wheel | Scroll the visible range |
| Left / Right at the edge of the view | Scroll the view along with the selection |

The "Visible Range" sliders keep working and follow the zoom level.

## Curve tools

- **Line between selected points** (button, point context menu or `L`): puts
  all points between the two outermost selected points on a straight line. The
  two outer points stay where they are.
- **Offset of the whole base curve**: sets every point of the curve to its base
  frequency plus the entered offset. This replaces earlier edits instead of
  adding to them. The allowed range comes from the offset limits reported by
  the driver.
- **Flatten selection** and **Flatten curve to the right** in the point context
  menu set the affected points to the frequency of the clicked point.

Changes only reach the GPU after pressing "Apply" and confirming them.
