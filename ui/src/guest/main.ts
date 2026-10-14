import { mount } from 'svelte';
import '../styles/base.css';
import App from './App.svelte';

mount(App, { target: document.getElementById('app') as HTMLElement });
