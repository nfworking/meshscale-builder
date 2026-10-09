'use server';

import { redirect } from 'next/navigation';

export async function submit(formData) {
  redirect('/action?done=' + encodeURIComponent(String(formData.get('name') || '')));
}
