# Third-party notices

alpha2 is under the MIT License (`LICENSE.md`). It also contains third-party work, under the terms below.

## Rust crates

The `alpha2` npm package and the native archives each list the third-party Rust crates compiled into them, with their license texts: `wasm/rust-crate-licenses.md` in the package, `licenses/rust-crate-licenses.md` in each archive.

## FFmpeg

The native archives load FFmpeg 8, built as shared libraries under the LGPL 2.1 or later with its GPL and nonfree components disabled. Each archive carries the license texts of FFmpeg and the libraries it is built with in `licenses/`, and says where to get the corresponding source in `compliance/SOURCE.md`.

## DirectX Shader Compiler

The Windows archive (and the wheel and Node install built from it) carries `bin/dxcompiler.dll` from Microsoft's DirectX Shader Compiler release v1.9.2609 (https://github.com/microsoft/DirectXShaderCompiler/releases/tag/v1.9.2609), which `alpha2-host` loads to compile its DX12 shaders. It is under the University of Illinois/NCSA Open Source License (the LLVM release license), the MIT License, and the Microsoft Software License Terms for the DirectX Shader Compiler; the texts are in `licenses/DirectXShaderCompiler/`, and `compliance/DXC.toml` records the release and the checksums of what was shipped.

## OpenH264

alpha2 does not include OpenH264. On request (`python -m alpha2 install --openh264`, `npx @alpha2-sh/node install --openh264`) it downloads Cisco's prebuilt OpenH264 binary from Cisco to the user's machine, and `alpha2-host` loads it to encode H.264 in software; `ALPHA2_OPENH264=0` turns it off. Cisco's binary comes with the licence below (https://www.openh264.org/BINARY_LICENSE.txt, v1.0, its typographic quotes and copyright sign written in ASCII).

OpenH264 Video Codec provided by Cisco Systems, Inc.

```
-------------------------------------------------------
About The Cisco-Provided Binary of OpenH264 Video Codec
-------------------------------------------------------

Cisco provides this program under the terms of the BSD license.

Additionally, this binary is licensed under Cisco's AVC/H.264 Patent Portfolio License from MPEG LA, at no cost to you, provided that the requirements and conditions shown below in the AVC/H.264 Patent Portfolio sections are met.

As with all AVC/H.264 codecs, you may also obtain your own patent license from MPEG LA or from the individual patent owners, or proceed at your own risk.  Your rights from Cisco under the BSD license are not affected by this choice.

For more information on the OpenH264 binary licensing, please see the OpenH264 FAQ found at http://www.openh264.org/faq.html#binary

A corresponding source code to this binary program is available under the same BSD terms, which can be found at http://www.openh264.org

-----------
BSD License
-----------

Copyright (c) 2014 Cisco Systems, Inc.

All rights reserved.

Redistribution and use in source and binary forms, with or without modification, are permitted provided that the following conditions are met:

1. Redistributions of source code must retain the above copyright notice, this list of conditions and the following disclaimer.

2. Redistributions in binary form must reproduce the above copyright notice, this list of conditions and the following disclaimer in the documentation and/or other materials provided with the distribution.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE DISCLAIMED.  IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

-----------------------------------------
AVC/H.264 Patent Portfolio License Notice
-----------------------------------------

The binary form of this Software is distributed by Cisco under the AVC/H.264 Patent Portfolio License from MPEG LA, and is subject to the following requirements, which may or may not be applicable to your use of this software:

THIS PRODUCT IS LICENSED UNDER THE AVC PATENT PORTFOLIO LICENSE FOR THE PERSONAL USE OF A CONSUMER OR OTHER USES IN WHICH IT DOES NOT RECEIVE REMUNERATION TO (i) ENCODE VIDEO IN COMPLIANCE WITH THE AVC STANDARD ("AVC VIDEO") AND/OR (ii) DECODE AVC VIDEO THAT WAS ENCODED BY A CONSUMER ENGAGED IN A PERSONAL ACTIVITY AND/OR WAS OBTAINED FROM A VIDEO PROVIDER LICENSED TO PROVIDE AVC VIDEO.  NO LICENSE IS GRANTED OR SHALL BE IMPLIED FOR ANY OTHER USE.  ADDITIONAL INFORMATION MAY BE OBTAINED FROM MPEG LA, L.L.C. SEE HTTP://WWW.MPEGLA.COM

Accordingly, please be advised that content providers and broadcasters using AVC/H.264 in their service may be required to obtain a separate use license from MPEG LA, referred to as "(b) sublicenses" in the SUMMARY OF AVC/H.264 LICENSE TERMS from MPEG LA found at http://www.openh264.org/mpegla

---------------------------------------------
AVC/H.264 Patent Portfolio License Conditions
---------------------------------------------

In addition, the Cisco-provided binary of this Software is licensed under Cisco's license from MPEG LA only if the following conditions are met:

1. The Cisco-provided binary is separately downloaded to an end user's device, and not integrated into or combined with third party software prior to being downloaded to the end user's device;

2. The end user must have the ability to control (e.g., to enable, disable, or re-enable) the use of the Cisco-provided binary;

3. Third party software, in the location where end users can control the use of the Cisco-provided binary, must display the following text:

       "OpenH264 Video Codec provided by Cisco Systems, Inc."

4.  Any third-party software that makes use of the Cisco-provided binary must reproduce all of the above text, as well as this last condition, in the EULA and/or in another location where licensing information is to be presented to the end user.

                          v1.0
```

## STIX Two Math and the dvips color names

The `alpha2-extensions` crate, the extensions wasm module in the `alpha2` npm package, and the native archives contain the STIX Two Math font (version 2.13) and a modified copy of the dvips color names, both embedded by the `latex-rust` crate (https://github.com/jscarr64/latex-rust) to typeset LaTeX math. The font is under the SIL Open Font License 1.1, below with its copyright and trademark signs and dashes written in ASCII.

The color names are a modified copy of `dvipsnam.def` from the `%<*dvipsnames>` section of `drivers.dtx` in the LaTeX graphics bundle, Copyright 1994 David Carlisle and Sebastian Rahtz, Copyright 1995-1999 David Carlisle, Copyright 2000-2026 The LaTeX Project, under the LaTeX Project Public License 1.3c or later (https://www.latex-project.org/lppl.txt). The definitions were rewritten as a tab-separated table and re-sorted by name; the original authors and The LaTeX Project do not maintain or support it. The unmodified graphics bundle is at https://github.com/latex3/latex2e/tree/develop/required/graphics (CTAN: macros/latex/required/graphics).

```
Copyright 2001-2021 The STIX Fonts Project Authors (https://github.com/stipub/stixfonts)

STIX Fonts(TM) is a trademark of The Institute of Electrical and
Electronics Engineers, Inc.

Portions copyright (c) 1998-2003 by MicroPress, Inc.
(www.micropress-inc.com), with Reserved Font Name TM Math.
To obtain additional mathematical fonts, please contact
    MicroPress, Inc.,
    68-30 Harrow Street,
    Forest Hills, NY 11375,
    USA
    Phone: (718) 575-1816.  

Portions copyright (c) 1990 by Elsevier, Inc.  

Portions copyright (c) 2010, 2012, 2014 Adobe Systems Incorporated.

This Font Software is licensed under the SIL Open Font License, Version 1.1.
This license is copied below, and is also available with a FAQ at:
http://scripts.sil.org/OFL

-----------------------------------------------------------
SIL OPEN FONT LICENSE Version 1.1 - 26 February 2007
-----------------------------------------------------------

PREAMBLE
The goals of the Open Font License (OFL) are to stimulate worldwide
development of collaborative font projects, to support the font creation
efforts of academic and linguistic communities, and to provide a free and
open framework in which fonts may be shared and improved in partnership
with others.

The OFL allows the licensed fonts to be used, studied, modified and
redistributed freely as long as they are not sold by themselves. The
fonts, including any derivative works, can be bundled, embedded, 
redistributed and/or sold with any software provided that any reserved
names are not used by derivative works. The fonts and derivatives,
however, cannot be released under any other type of license. The
requirement for fonts to remain under this license does not apply
to any document created using the fonts or their derivatives.

DEFINITIONS
"Font Software" refers to the set of files released by the Copyright
Holder(s) under this license and clearly marked as such. This may
include source files, build scripts and documentation.

"Reserved Font Name" refers to any names specified as such after the
copyright statement(s).

"Original Version" refers to the collection of Font Software components as
distributed by the Copyright Holder(s).

"Modified Version" refers to any derivative made by adding to, deleting,
or substituting -- in part or in whole -- any of the components of the
Original Version, by changing formats or by porting the Font Software to a
new environment.

"Author" refers to any designer, engineer, programmer, technical
writer or other person who contributed to the Font Software.

PERMISSION & CONDITIONS
Permission is hereby granted, free of charge, to any person obtaining
a copy of the Font Software, to use, study, copy, merge, embed, modify,
redistribute, and sell modified and unmodified copies of the Font
Software, subject to the following conditions:

1) Neither the Font Software nor any of its individual components,
in Original or Modified Versions, may be sold by itself.

2) Original or Modified Versions of the Font Software may be bundled,
redistributed and/or sold with any software, provided that each copy
contains the above copyright notice and this license. These can be
included either as stand-alone text files, human-readable headers or
in the appropriate machine-readable metadata fields within text or
binary files as long as those fields can be easily viewed by the user.

3) No Modified Version of the Font Software may use the Reserved Font
Name(s) unless explicit written permission is granted by the corresponding
Copyright Holder. This restriction only applies to the primary font name as
presented to the users.

4) The name(s) of the Copyright Holder(s) or the Author(s) of the Font
Software shall not be used to promote, endorse or advertise any
Modified Version, except to acknowledge the contribution(s) of the
Copyright Holder(s) and the Author(s) or with their explicit written
permission.

5) The Font Software, modified or unmodified, in part or in whole,
must be distributed entirely under this license, and must not be
distributed under any other license. The requirement for fonts to
remain under this license does not apply to any document created
using the Font Software.

TERMINATION
This license becomes null and void if any of the above conditions are
not met.

DISCLAIMER
THE FONT SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO ANY WARRANTIES OF
MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT
OF COPYRIGHT, PATENT, TRADEMARK, OR OTHER RIGHT. IN NO EVENT SHALL THE
COPYRIGHT HOLDER BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
INCLUDING ANY GENERAL, SPECIAL, INDIRECT, INCIDENTAL, OR CONSEQUENTIAL
DAMAGES, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
FROM, OUT OF THE USE OR INABILITY TO USE THE FONT SOFTWARE OR FROM
OTHER DEALINGS IN THE FONT SOFTWARE.
```

## PDF standard fonts and CMaps

The `alpha2-extensions` crate, the extensions wasm module in the `alpha2` npm package, and the native archives contain data the `hayro` crates (https://github.com/LaurenzV/hayro) embed to draw PDF pages: substitutes for the 14 standard PDF fonts, extracted from PDFium (original code copyright 2014 Foxit Software Inc.), and Adobe's predefined CMaps.

The standard font substitutes:

```
Copyright 2014 PDFium Authors. All rights reserved.

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are
met:

   * Redistributions of source code must retain the above copyright
notice, this list of conditions and the following disclaimer.
   * Redistributions in binary form must reproduce the above
copyright notice, this list of conditions and the following disclaimer
in the documentation and/or other materials provided with the
distribution.
   * Neither the name of Google Inc. nor the names of its
contributors may be used to endorse or promote products derived from
this software without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
"AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT
OWNER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
(INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```

The CMaps:

```
Copyright 1990-2023 Adobe. All rights reserved.

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are
met:

Redistributions of source code must retain the above copyright notice,
this list of conditions and the following disclaimer.

Redistributions in binary form must reproduce the above copyright
notice, this list of conditions and the following disclaimer in the
documentation and/or other materials provided with the distribution.

Neither the name of Adobe nor the names of its contributors may be
used to endorse or promote products derived from this software without
specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
"AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT
HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
(INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```

## gl-transitions

The transitions in `crates/alpha2-shaders/shaders/runtime/transitions/` are WGSL ports of shaders from [gl-transitions](https://github.com/gl-transitions/gl-transitions) at commit `902218a1b63773ac0d0d9f491951da3392365bfe`. Each file names the shader it ports and its author.

Copyright (c) 2017-present gl-transitions contributors.

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
