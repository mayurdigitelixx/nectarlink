// SPDX-License-Identifier: GPL-3.0-or-later
package app.nectarlink.android.ui

import android.view.View
import android.view.ViewGroup

/**
 * This view, taken off the parent it still has. For an `AndroidView` whose
 * view outlives it (a remembered camera preview): when the screen rebuilds
 * that part, the new `AndroidView` can be made before the old one lets go,
 * and adding a view that has a parent crashes.
 */
fun <T : View> T.detached(): T = apply { (parent as? ViewGroup)?.removeView(this) }
