/*
 * Copyright (c) 2025 Epic Games, Inc. All Rights Reserved.
 *
 * Redistribution and use in source and binary forms, with or without
 * modification, are permitted provided that the following conditions
 * are met:
 * 1. Redistributions of source code must retain the above copyright
 *    notice, this list of conditions and the following disclaimer.
 * 2. Redistributions in binary form must reproduce the above copyright
 *    notice, this list of conditions and the following disclaimer in the
 *    documentation and/or other materials provided with the distribution.
 *
 * THIS SOFTWARE IS PROVIDED BY EPIC GAMES, INC. ``AS IS'' AND ANY
 * EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
 * IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR
 * PURPOSE ARE DISCLAIMED.  IN NO EVENT SHALL EPIC GAMES, INC. OR
 * CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL,
 * EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO,
 * PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR
 * PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY
 * OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
 * (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
 * OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE. 
 */

#include <unwind.h>
#include <pizlonated_eh_landing_pad.h>
#include <stdfil.h>

_Unwind_Reason_Code __gcc_personality_v0(
    int version, _Unwind_Action actions, _Unwind_Exception_Class exception_class,
    struct _Unwind_Exception* exception_object, struct _Unwind_Context* context)
{
    ZASSERT(version == 1);
    (void)exception_class;
    
    if (actions & _UA_SEARCH_PHASE)
        return _URC_CONTINUE_UNWIND;

    const pizlonated_eh_landing_pad* lsda = (const pizlonated_eh_landing_pad*)
        _Unwind_GetLanguageSpecificData(context);
    if (!lsda)
        return _URC_CONTINUE_UNWIND;

    /* C code should only have a single cleanup action. */
    ZASSERT(pizlonated_eh_landing_pad_get_num_actions(lsda) == 1);
    ZASSERT(pizlonated_eh_encoded_action_is_cleanup(
                pizlonated_eh_landing_pad_get_encoded_action(lsda, 0)));
    
    _Unwind_SetGR(context, 0, exception_object);
    _Unwind_SetGR(context, 1, (void*)0);
    return _URC_INSTALL_CONTEXT;
}
