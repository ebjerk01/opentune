package org.opentune.app

import android.app.Activity
import android.os.Build
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.dynamicDarkColorScheme
import androidx.compose.material3.dynamicLightColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext

// A restrained, tool-like palette (teal primary, amber accent) that reads in
// both light and dark. Dynamic colour is used on Android 12+ when available.
private val LightColors = lightColorScheme(
    primary = Color(0xFF00696E),
    secondary = Color(0xFF4A6365),
    tertiary = Color(0xFF8A5A00),
    error = Color(0xFFBA1A1A),
)

private val DarkColors = darkColorScheme(
    primary = Color(0xFF4DD9DF),
    secondary = Color(0xFFB1CBCD),
    tertiary = Color(0xFFFFB95C),
    error = Color(0xFFFFB4AB),
)

@Composable
fun OpenTuneTheme(
    darkTheme: Boolean = isSystemInDarkTheme(),
    content: @Composable () -> Unit,
) {
    val context = LocalContext.current
    val colors = when {
        Build.VERSION.SDK_INT >= Build.VERSION_CODES.S ->
            if (darkTheme) dynamicDarkColorScheme(context) else dynamicLightColorScheme(context)
        darkTheme -> DarkColors
        else -> LightColors
    }
    MaterialTheme(colorScheme = colors, content = content)
}
