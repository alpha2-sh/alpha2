# Speech fixture

`speech.wav` is six phrases (16 kHz, mono, 16-bit) from the LibriVox recording of Sun Tzu's "The
Art of War", chapters 3 and 4, which is in the public domain. `speech.json` names the source file
and where each phrase was cut, at a quiet point on either side and with no pause of 180 ms or more
inside. Tests insert silences of known lengths between the phrases, so where speech is is known by
construction.
